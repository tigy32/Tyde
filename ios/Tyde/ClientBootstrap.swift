import CryptoKit
import Foundation
import WebKit

struct NativeBootstrapManifest: Decodable {
    let release: String
    let bootstrap_version: UInt32
    let bridge_version: UInt32
    let bootstrap_url: String
    let bridge_sha256: String
}

enum ClientBootstrap {
    static let origin = URL(string: nativeBootstrapOrigin)!
    static let entry = URL(string: "/tyde/?native=1", relativeTo: origin)!.absoluteURL

    static func permits(_ origin: WKSecurityOrigin) -> Bool {
        origin.protocol == "https" && origin.host == self.origin.host
            && (origin.port == 0 ? 443 : origin.port) == (self.origin.port ?? 443)
    }

    static func permits(_ url: URL) -> Bool {
        url.scheme == "https" && url.host == origin.host && (url.port ?? 443) == (origin.port ?? 443)
            && url.user == nil && url.password == nil
    }

    static func permitsEntry(_ url: URL) -> Bool {
        guard permits(url),
              let requested = URLComponents(url: url, resolvingAgainstBaseURL: false),
              let expected = URLComponents(url: entry, resolvingAgainstBaseURL: false) else { return false }
        // URL.path normalizes the directory suffix; admission uses the exact URI components.
        return requested.percentEncodedPath == expected.percentEncodedPath
            && requested.percentEncodedQuery == expected.percentEncodedQuery
    }

    static func load() throws -> (URL, String) {
        guard let root = Bundle.main.resourceURL?.appendingPathComponent("Web", isDirectory: true),
              let bridge = Bundle.main.url(forResource: "native-host", withExtension: "js") else {
            throw HostError("Native bootstrap resources are missing")
        }
        let manifest = try JSONDecoder().decode(NativeBootstrapManifest.self,
            from: Data(contentsOf: root.appendingPathComponent("native-manifest.json")))
        guard manifest.release == nativeShellRelease, manifest.bridge_version == nativeBridgeVersion,
              manifest.bootstrap_version == nativeBootstrapVersion,
              manifest.bootstrap_url == entry.absoluteString else {
            throw HostError("Native bootstrap manifest is incompatible")
        }
        let data = try Data(contentsOf: bridge)
        let actual = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
        guard actual == manifest.bridge_sha256 else { throw HostError("Native bridge integrity check failed") }
        guard let script = String(data: data, encoding: .utf8) else { throw HostError("Native bridge encoding is invalid") }
        return (entry, script)
    }
}

struct HostError: LocalizedError {
    let errorDescription: String?
    init(_ message: String) { errorDescription = message }
}
