import AuthenticationServices
import Foundation
import OSLog
import Security
import UIKit
import WebKit

@MainActor
final class AuthenticationAnchor: NSObject, ASWebAuthenticationPresentationContextProviding {
    let window: UIWindow
    init(window: UIWindow) { self.window = window }
    func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor { window }
}

@MainActor
final class PlatformBridge: NSObject, WKScriptMessageHandlerWithReply {
    var onPresentation: ((NativePresentation) -> Void)?
    weak var presentingController: UIViewController?
    private enum AuthenticationState {
        case idle
        case starting
        case active(ASWebAuthenticationSession, AuthenticationAnchor)
    }
    private var authentication = AuthenticationState.idle
    private var httpTasks: [UInt32: Task<NativeHttpResponse, Error>] = [:]
    private let networking = NativeNetworking()
    private let logger = Logger(subsystem: "dev.tyde.mobile.nativeqa", category: "platform")

    func userContentController(_ userContentController: WKUserContentController, didReceive message: WKScriptMessage, replyHandler: @escaping (Any?, String?) -> Void) {
        guard message.frameInfo.isMainFrame,
              message.frameInfo.securityOrigin.protocol == "http",
              message.frameInfo.securityOrigin.host == "127.0.0.1",
              message.frameInfo.securityOrigin.port == 18736,
              let endpoint = NativeEndpoint(rawValue: message.name),
              let json = message.body as? String, let data = json.data(using: .utf8) else {
            replyHandler(nil, "Untrusted native bridge message")
            return
        }
        let decoder = JSONDecoder()
        do {
            switch endpoint {
            case .http:
                let request = try decoder.decode(NativeHttpRequest.self, from: data)
                guard httpTasks[request.id] == nil else { throw HostError("Duplicate native HTTP request") }
                let task = Task { try await networking.send(request) }
                httpTasks[request.id] = task
                Task { @MainActor in
                    defer { httpTasks.removeValue(forKey: request.id) }
                    do {
                        let object = try JSONSerialization.jsonObject(with: JSONEncoder().encode(try await task.value))
                        replyHandler(object, nil)
                    } catch { reject(endpoint, error, replyHandler) }
                }
                return
            case .http_cancel:
                let request = try decoder.decode(NativeHttpCancel.self, from: data)
                // Completion may settle before the frame's cancellation message arrives.
                httpTasks[request.id]?.cancel()
                replyHandler(NSNull(), nil)
                return
            default: break
            }
        } catch { reject(endpoint, error, replyHandler); return }
        Task { @MainActor in
            do {
                switch endpoint {
                case .presentation:
                    let state = try decoder.decode(NativePresentation.self, from: data)
                    guard state.version == nativeBridgeVersion else { throw HostError("Native bridge version mismatch") }
                    onPresentation?(state)
                    replyHandler(NSNull(), nil)
                case .http, .http_cancel:
                    throw HostError("Native HTTP dispatch invariant failed")
                case .secret_read:
                    let key = try decoder.decode(NativeSecretKey.self, from: data)
                    replyHandler(try readSecret(key.key).map { $0 as Any } ?? NSNull(), nil)
                case .secret_write:
                    let value = try decoder.decode(NativeSecretWrite.self, from: data)
                    try writeSecret(key: value.key, value: value.value)
                    replyHandler(NSNull(), nil)
                case .secret_delete:
                    let key = try decoder.decode(NativeSecretKey.self, from: data)
                    try deleteSecret(key.key)
                    replyHandler(NSNull(), nil)
                case .authenticate:
                    let request = try decoder.decode(NativeAuthRequest.self, from: data)
                    let callback = try await authenticate(request)
                    replyHandler(callback, nil)
                }
            } catch { reject(endpoint, error, replyHandler) }
        }
    }

    private func reject(_ endpoint: NativeEndpoint, _ error: Error, _ replyHandler: (Any?, String?) -> Void) {
        let reason: String
        switch error {
        case let error as HostError: reason = error.localizedDescription
        case let error as URLError: reason = "Native network operation failed (code \(error.code.rawValue))"
        case is DecodingError: reason = "Native request could not be decoded"
        case is EncodingError: reason = "Native response could not be encoded"
        case is CancellationError: reason = "Native operation was cancelled"
        default:
            let error = error as NSError
            reason = "Native operation failed (domain \(error.domain), code \(error.code))"
        }
        // Only controlled reasons and framework codes; never keys, values, URLs or userInfo.
        logger.error("Native \(endpoint.rawValue, privacy: .public) failed: \(reason, privacy: .public)")
        replyHandler(nil, reason)
    }

    private func secretQuery(_ key: String) throws -> [String: Any] {
        guard key.hasPrefix("tyde-web-"), key.count < 200 else { throw HostError("Invalid pairing secret key") }
        guard let service = Bundle.main.bundleIdentifier else { throw HostError("Native app identity is missing") }
        return [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service, kSecAttrAccount as String: key]
    }

    private func readSecret(_ key: String) throws -> String? {
        var query = try secretQuery(key)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess else {
            throw HostError("Pairing secret could not be read from Keychain (OSStatus \(status))")
        }
        guard let data = result as? Data, let value = String(data: data, encoding: .utf8) else {
            throw HostError("Pairing secret in Keychain is not valid UTF-8 data")
        }
        return value
    }

    private func writeSecret(key: String, value: String) throws {
        let query = try secretQuery(key)
        let values: [String: Any] = [kSecValueData as String: Data(value.utf8), kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly]
        var status = SecItemUpdate(query as CFDictionary, values as CFDictionary)
        if status == errSecItemNotFound {
            status = SecItemAdd(query.merging(values) { _, new in new } as CFDictionary, nil)
        }
        guard status == errSecSuccess else { throw HostError("Pairing secret could not be saved to Keychain (OSStatus \(status))") }
    }

    private func deleteSecret(_ key: String) throws {
        let status = SecItemDelete(try secretQuery(key) as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else { throw HostError("Pairing secret could not be removed from Keychain (OSStatus \(status))") }
    }

    #if DEBUG
    func checkSecureStorage() throws {
        let key = "tyde-web-platform-check-" + UUID().uuidString
        try writeSecret(key: key, value: "native-boundary-first")
        guard try readSecret(key) == "native-boundary-first" else { throw HostError("Native Keychain read did not match the written check value") }
        try writeSecret(key: key, value: "native-boundary-updated")
        guard try readSecret(key) == "native-boundary-updated" else { throw HostError("Native Keychain update did not persist the check value") }
        try deleteSecret(key)
        guard try readSecret(key) == nil else { throw HostError("Native Keychain deletion left the check value present") }
    }
    #endif

    private func authenticate(_ request: NativeAuthRequest) async throws -> String {
        guard case .idle = authentication,
              let window = presentingController?.viewIfLoaded?.window,
              window.windowScene?.activationState == .foregroundActive else {
            throw HostError("A native sign-in is already active or no app window is available")
        }
        let anchor = AuthenticationAnchor(window: window)
        authentication = .starting
        let url: URL
        do { url = try await networking.startAuthentication(provider: request.provider) }
        catch { authentication = .idle; throw error }
        guard presentingController?.viewIfLoaded?.window === anchor.window,
              anchor.window.windowScene?.activationState == .foregroundActive else {
            authentication = .idle
            throw HostError("Native sign-in needs the active app window")
        }
        return try await withCheckedThrowingContinuation { continuation in
            let session = ASWebAuthenticationSession(url: url, callback: .customScheme("tyde-mobile")) { [weak self] callback, error in
                Task { @MainActor in
                    self?.authentication = .idle
                    guard let callback, callback.scheme == "tyde-mobile", callback.host == "auth", callback.path == "/callback" else {
                        continuation.resume(throwing: HostError(error == nil ? "Native authentication callback is invalid" : "Native sign-in was cancelled or failed"))
                        return
                    }
                    continuation.resume(returning: callback.absoluteString)
                }
            }
            session.presentationContextProvider = anchor
            session.prefersEphemeralWebBrowserSession = true
            authentication = .active(session, anchor)
            if !session.start() {
                authentication = .idle
                continuation.resume(throwing: HostError("Native sign-in could not start"))
            }
        }
    }

}

/// The app's cookie jar is private to its sandbox; HTTP-only session cookies never enter JavaScript.
@MainActor
final class NativeNetworking: NSObject, URLSessionTaskDelegate {
    private lazy var session = URLSession(configuration: .default, delegate: self, delegateQueue: nil)

    func startAuthentication(provider: NativeAuthProvider) async throws -> URL {
        var components = URLComponents(string: "https://tycode.dev/api/tyde/mobile/v1/auth/native/start")!
        components.queryItems = [URLQueryItem(name: "provider", value: provider.rawValue)]
        let (_, response) = try await session.data(for: URLRequest(url: components.url!))
        guard let response = response as? HTTPURLResponse else { throw HostError("Native authentication start returned no HTTP response") }
        guard response.statusCode == 303 else {
            throw HostError("Native sign-in service refused to start (HTTP \(response.statusCode))")
        }
        guard let location = response.value(forHTTPHeaderField: "Location"), let url = URL(string: location),
              url.scheme == "https", url.port == nil, url.user == nil, url.password == nil, url.fragment == nil else {
            throw HostError("Native authentication authorization URL is invalid")
        }
        let expectedHost = provider == .google ? "accounts.google.com" : "appleid.apple.com"
        let expectedPath = provider == .google ? "/o/oauth2/v2/auth" : "/auth/authorize"
        guard url.host == expectedHost, url.path == expectedPath else {
            throw HostError("Native authentication returned an unexpected provider destination")
        }
        return url
    }

    func send(_ input: NativeHttpRequest) async throws -> NativeHttpResponse {
        guard let url = URL(string: input.url), url.scheme == "https", url.user == nil, url.password == nil, url.fragment == nil,
              (url.host == "tycode.dev" && url.port == nil && url.path.hasPrefix("/api/tyde/mobile/v1/"))
                || (url.host != nil && url.path == "/tyde/pair" && input.method == .post) else {
            throw HostError("Native HTTP destination is not permitted")
        }
        var request = URLRequest(url: url)
        request.httpMethod = input.method == .get ? "GET" : "POST"
        request.httpBody = input.body.map { Data($0.utf8) }
        for (name, value) in input.headers {
            guard ["accept", "content-type", "authorization", "x-tycode-pairing-auth"].contains(name.lowercased()) else {
                throw HostError("Native HTTP header is not permitted")
            }
            request.setValue(value, forHTTPHeaderField: name)
        }
        let (data, response) = try await session.data(for: request)
        guard let response = response as? HTTPURLResponse, let body = String(data: data, encoding: .utf8),
              let status = UInt16(exactly: response.statusCode) else { throw HostError("Native HTTP response is invalid") }
        return NativeHttpResponse(status: status, body: body)
    }

    nonisolated func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void) {
        completionHandler(nil)
    }
}
