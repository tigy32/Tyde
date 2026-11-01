import CryptoKit
import Foundation
import Network

enum AssetFormat: UInt32, Decodable {
    case splitOriginV1 = 1
}

struct BundledDocument {
    let assetOrigin: URL
    let transportPort: UInt16
    let response: HTTPURLResponse
    let data: Data
}

struct AssetManifest: Decodable {
    let asset_format: AssetFormat
    let release: String
    let protocol_version: UInt32
    let bridge_version: UInt32
    let csp: String
    let files: [String: String]
}

/// Only immutable application resources are served. No Tyde host runs on the phone.
final class AssetServer {
    static let origin = URL(string: "http://127.0.0.1:18736")!
    private static let originToken = "__TYDE_NATIVE_ASSET_ORIGIN__"
    private let queue = DispatchQueue.main
    private var listener: NWListener?
    private var connections: [UUID: NWConnection] = [:]
    private var resources: [String: Data] = [:]
    private var documentTemplate = ""
    private var assetOrigin: URL?

    func start(onReady: @escaping (BundledDocument) -> Void, onFailure: @escaping (Error) -> Void) throws {
        guard let root = Bundle.main.resourceURL?.appendingPathComponent("Web", isDirectory: true) else {
            throw HostError("Bundled web resources are missing")
        }
        let manifest = try JSONDecoder().decode(AssetManifest.self, from: Data(contentsOf: root.appendingPathComponent("native-manifest.json")))
        guard manifest.asset_format == .splitOriginV1, manifest.bridge_version == nativeBridgeVersion, manifest.release == nativeClientRelease,
              manifest.protocol_version == nativeProtocolVersion,
              manifest.files["index.html"] != nil else {
            throw HostError("Bundled client manifest is incompatible")
        }
        for (path, expected) in manifest.files {
            guard !path.hasPrefix("/"), !path.split(separator: "/").contains("..") else {
                throw HostError("Invalid bundled asset path")
            }
            let data = try Data(contentsOf: root.appendingPathComponent(path))
            let actual = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
            guard actual == expected else { throw HostError("Bundled asset integrity check failed") }
            if path == "index.html" {
                guard let html = String(data: data, encoding: .utf8),
                      html.contains(Self.originToken), manifest.csp.contains(Self.originToken),
                      html.contains("http-equiv=\"Content-Security-Policy\"") else {
                    throw HostError("Bundled document transport template is invalid")
                }
                documentTemplate = html
            } else { resources["/" + path] = data }
        }
        let parameters = NWParameters.tcp
        parameters.requiredLocalEndpoint = .hostPort(host: "127.0.0.1", port: .any)
        let listener = try NWListener(using: parameters)
        self.listener = listener
        var ready = false
        listener.stateUpdateHandler = { [weak self, weak listener] state in
            guard let self, let listener else { return }
            switch state {
            case .ready:
                guard !ready else { return }
                do {
                    guard let port = listener.port, port.rawValue != 0,
                          let origin = URL(string: "http://127.0.0.1:\(port.rawValue)") else {
                        throw HostError("Bundled resource listener has no assigned endpoint")
                    }
                    self.assetOrigin = origin
                    let html = self.documentTemplate.replacingOccurrences(of: Self.originToken, with: origin.absoluteString)
                    guard !html.contains(Self.originToken),
                          let response = HTTPURLResponse(url: Self.origin.appendingPathComponent("index.html"), statusCode: 200,
                              httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "text/html; charset=utf-8", "Cache-Control": "no-store"]) else {
                        throw HostError("Bundled document could not be prepared")
                    }
                    ready = true
                    onReady(BundledDocument(assetOrigin: origin, transportPort: port.rawValue, response: response, data: Data(html.utf8)))
                } catch {
                    listener.cancel()
                    onFailure(error)
                }
            case .failed(let error):
                onFailure(error)
            default: return
            }
        }
        listener.newConnectionHandler = { [weak self] connection in
            guard let self else { connection.cancel(); return }
            let id = UUID()
            self.connections[id] = connection
            connection.start(queue: self.queue)
            self.receive(connection, id: id, bytes: Data())
        }
        listener.start(queue: queue)
    }

    private func receive(_ connection: NWConnection, id: UUID, bytes: Data) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 16384) { [weak self] data, _, complete, error in
            guard let self else { connection.cancel(); return }
            var bytes = bytes
            if let data { bytes.append(data) }
            if bytes.count > 16384 || error != nil || complete {
                self.finish(connection, id: id)
                return
            }
            guard let text = String(data: bytes, encoding: .utf8), text.contains("\r\n\r\n") else {
                self.receive(connection, id: id, bytes: bytes)
                return
            }
            let lines = text.components(separatedBy: "\r\n")
            let request = lines[0].split(separator: " ")
            let headers = lines.dropFirst().compactMap { line -> (String, String)? in
                guard let colon = line.firstIndex(of: ":") else { return nil }
                return (line[..<colon].lowercased(), line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces))
            }
            let hosts = headers.filter { $0.0 == "host" }
            let origins = headers.filter { $0.0 == "origin" }
            guard let port = self.assetOrigin?.port,
                  request.count == 3, request[0] == "GET", hosts.count == 1, origins.count == 1,
                  hosts[0].1 == "127.0.0.1:\(port)", origins[0].1 == Self.origin.absoluteString else {
                self.send(connection, id: id, status: "403 Forbidden", path: "", data: Data())
                return
            }
            let path = String(request[1])
            guard let data = self.resources[path] else {
                self.send(connection, id: id, status: "404 Not Found", path: path, data: Data())
                return
            }
            self.send(connection, id: id, status: "200 OK", path: path, data: data)
        }
    }

    private func send(_ connection: NWConnection, id: UUID, status: String, path: String, data: Data) {
        let mime: String
        switch (path as NSString).pathExtension {
        case "html": mime = "text/html; charset=utf-8"
        case "js": mime = "text/javascript; charset=utf-8"
        case "css": mime = "text/css; charset=utf-8"
        case "wasm": mime = "application/wasm"
        case "json": mime = "application/json"
        case "svg": mime = "image/svg+xml"
        case "png": mime = "image/png"
        case "woff2": mime = "font/woff2"
        default: mime = "application/octet-stream"
        }
        let head = "HTTP/1.1 \(status)\r\nContent-Type: \(mime)\r\nContent-Length: \(data.count)\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nAccess-Control-Allow-Origin: \(Self.origin.absoluteString)\r\nVary: Origin\r\nConnection: close\r\n\r\n"
        connection.send(content: Data(head.utf8) + data, completion: .contentProcessed { [weak self] _ in
            self?.finish(connection, id: id)
        })
    }

    private func finish(_ connection: NWConnection, id: UUID) {
        connection.cancel()
        connections.removeValue(forKey: id)
    }

    deinit { listener?.cancel(); connections.values.forEach { $0.cancel() } }
}

struct HostError: LocalizedError {
    let errorDescription: String?
    init(_ message: String) { errorDescription = message }
}
