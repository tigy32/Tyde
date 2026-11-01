import UIKit
#if DEBUG
import OSLog
#endif

@main
final class AppDelegate: UIResponder, UIApplicationDelegate {
    func application(_ application: UIApplication, configurationForConnecting session: UISceneSession, options: UIScene.ConnectionOptions) -> UISceneConfiguration {
        let config = UISceneConfiguration(name: "Tyde", sessionRole: session.role)
        config.delegateClass = SceneDelegate.self
        return config
    }
}

final class SceneDelegate: UIResponder, UIWindowSceneDelegate {
    var window: UIWindow?
    private var host: TydeHostController?

    func scene(_ scene: UIScene, willConnectTo session: UISceneSession, options: UIScene.ConnectionOptions) {
        guard let scene = scene as? UIWindowScene else { return }
        let window: UIWindow
        let host = TydeHostController()
        #if DEBUG
        if ProcessInfo.processInfo.environment["TYDE_IOS_NAVIGATION_DIAGNOSTICS"] == "1" {
            do {
                let diagnostics = try NativeNavigationDiagnostics()
                let observed = NativeNavigationWindow(windowScene: scene)
                observed.diagnostics = diagnostics
                host.navigationDiagnostics = diagnostics
                window = observed
            } catch {
                let reason = error as NSError
                NSLog("Native navigation diagnostics could not start: domain=%@ code=%ld", reason.domain, reason.code)
                preconditionFailure("Native navigation diagnostics could not start")
            }
        } else { window = UIWindow(windowScene: scene) }
        #else
        window = UIWindow(windowScene: scene)
        #endif
        window.rootViewController = host
        self.host = host
        self.window = window
        window.makeKeyAndVisible()
        host.start()
    }

    func sceneDidEnterBackground(_ scene: UIScene) { host?.send(.background) }
    func sceneWillEnterForeground(_ scene: UIScene) { host?.send(.foreground) }
}

#if DEBUG
@MainActor
final class NativeNavigationWindow: UIWindow {
    var diagnostics: NativeNavigationDiagnostics?

    override func sendEvent(_ event: UIEvent) {
        diagnostics?.touches(.touch_will_dispatch, event: event, window: self)
        super.sendEvent(event)
        diagnostics?.touches(.touch_did_dispatch, event: event, window: self)
    }
}

@MainActor
final class NativeNavigationDiagnostics {
    enum Stage: String, Encodable {
        case projection, projection_applied, layout, intent_requested, script_completed
        case touch_will_dispatch, touch_did_dispatch
    }

    private struct Window: Encodable, Equatable {
        let frame: CGRect
        let bounds: CGRect
        let safeInsets: [CGFloat]
        let key: Bool
        let hidden: Bool
        let interaction: Bool
        let level: CGFloat
        let sceneBounds: CGRect?
        let screenBounds: CGRect?
        let screenCoordinateBounds: CGRect?
        let fixedScreenCoordinateBounds: CGRect?
        let rectInScreen: CGRect?
        let screenScale: CGFloat?
        let sceneActivation: Int?
        let interfaceOrientation: Int?

        init(_ window: UIWindow) {
            frame = window.frame
            bounds = window.bounds
            safeInsets = [window.safeAreaInsets.top, window.safeAreaInsets.left,
                          window.safeAreaInsets.bottom, window.safeAreaInsets.right]
            key = window.isKeyWindow
            hidden = window.isHidden
            interaction = window.isUserInteractionEnabled
            level = window.windowLevel.rawValue
            sceneBounds = window.windowScene?.coordinateSpace.bounds
            screenBounds = window.windowScene?.screen.bounds
            screenCoordinateBounds = window.windowScene?.screen.coordinateSpace.bounds
            fixedScreenCoordinateBounds = window.windowScene?.screen.fixedCoordinateSpace.bounds
            rectInScreen = window.windowScene.map { window.convert(window.bounds, to: $0.screen.coordinateSpace) }
            screenScale = window.windowScene?.screen.scale
            sceneActivation = window.windowScene?.activationState.rawValue
            interfaceOrientation = window.windowScene?.interfaceOrientation.rawValue
        }
    }

    private static let navigationIdentifiers = ["native-hosts", "native-new-chat", "native-tab-home",
                                                "native-tab-agents", "native-tab-sessions", "native-tab-settings"]

    private struct View: Encodable, Equatable {
        let type: String
        let control: String?
        let bounds: CGRect
        let rectInWindow: CGRect?
        let hidden: Bool
        let alpha: CGFloat
        let interaction: Bool
        let accessibilityElement: Bool
        let accessibilityHidden: Bool
        let selected: Bool?
        let accessibilitySelected: Bool

        init(_ view: UIView, window: UIWindow) {
            type = NSStringFromClass(Swift.type(of: view))
            let allowed = NativeNavigationDiagnostics.navigationIdentifiers + ["tyde-content"]
            control = view.accessibilityIdentifier.flatMap { allowed.contains($0) ? $0 : nil }
            bounds = view.bounds
            rectInWindow = view.window === window ? view.convert(view.bounds, to: window) : nil
            hidden = view.isHidden
            alpha = view.alpha
            interaction = view.isUserInteractionEnabled
            accessibilityElement = view.isAccessibilityElement
            accessibilityHidden = view.accessibilityElementsHidden
            selected = (view as? UIControl)?.isSelected
            accessibilitySelected = view.accessibilityTraits.contains(.selected)
        }
    }

    private struct NavigationControl: Encodable, Equatable {
        let identifier: String
        let view: View
        let ancestors: [View]?
        let accessibilityFrameInScreen: CGRect
        let activationPointInScreen: CGPoint
        let activationPointInWindow: CGPoint
        let activationHit: View?
        let activationHitNavigationAncestor: String?
    }

    private struct Touch: Encodable {
        let phase: Int
        let type: Int
        let point: CGPoint
        let belongsToWindow: Bool
        let target: [View]
        let hitTest: [View]
    }

    private struct Observation: Encodable {
        let sequence: Int
        let uptime: TimeInterval
        let stage: Stage
        let window: Window?
        let intent: NativeIntent?
        let ready: Bool?
        let tab: NativeTab?
        let nativeSelectedTab: NativeTab?
        let route: NativeRoute?
        let workspace: Bool?
        let modal: Bool?
        let scriptFailed: Bool?
        let eventTimestamp: TimeInterval?
        let touch: Touch?
        let navigationControls: [NavigationControl]?
    }

    private let file: FileHandle
    private let logger = Logger(subsystem: "dev.tyde.mobile.nativeqa", category: "navigation-diagnostics")
    private var sequence = 0
    private var failed = false
    private var lastLayout: Window?
    private var lastControls: [NavigationControl]?
    private var announced = false

    init() throws {
        let path = FileManager.default.temporaryDirectory.appendingPathComponent("native-navigation-diagnostics-\(UUID().uuidString).jsonl")
        guard FileManager.default.createFile(atPath: path.path, contents: nil, attributes: [.posixPermissions: 0o600]) else {
            throw HostError("Native navigation diagnostics file could not be created")
        }
        file = try FileHandle(forWritingTo: path)
    }

    func record(_ stage: Stage, window: UIWindow?, state: NativePresentation?, ready: Bool,
                intent: NativeIntent? = nil, scriptFailed: Bool? = nil,
                nativeSelectedTab: NativeTab? = nil) {
        let geometry = window.map(Window.init)
        let controls: [NavigationControl]?
        do { controls = (stage == .layout || stage == .projection_applied) ? try window.map(navigationControls) : nil }
        catch { fail(error); return }
        if stage == .layout {
            guard geometry != lastLayout || controls != lastControls || (!announced && ready) else { return }
            lastLayout = geometry
            lastControls = controls
        }
        append(Observation(sequence: sequence + 1, uptime: ProcessInfo.processInfo.systemUptime,
                           stage: stage, window: geometry, intent: intent, ready: ready,
                           tab: state?.tab, nativeSelectedTab: nativeSelectedTab,
                           route: state?.route, workspace: state?.workspace,
                           modal: state?.modal, scriptFailed: scriptFailed,
                           eventTimestamp: nil, touch: nil, navigationControls: controls))
        if !announced, !failed, stage == .layout, ready,
           geometry?.key == true, geometry?.hidden == false,
           geometry.map({ $0.bounds.width > 0 && $0.bounds.height > 0 }) == true,
           window?.windowScene?.activationState == .foregroundActive {
            announced = true
            logger.notice("Native navigation diagnostics ready; bounded passive trace available")
        }
    }

    func touches(_ stage: Stage, event: UIEvent, window: UIWindow) {
        guard event.type == .touches else { return }
        for touch in event.allTouches ?? [] {
            guard [.began, .ended, .cancelled].contains(touch.phase) else { continue }
            let point = touch.location(in: window)
            let hit = stage == .touch_will_dispatch ? window.hitTest(point, with: event) : nil
            let controls: [NavigationControl]?
            do { controls = stage == .touch_will_dispatch ? try navigationControls(window) : nil }
            catch { fail(error); return }
            let observed = Touch(phase: touch.phase.rawValue, type: touch.type.rawValue,
                                 point: point, belongsToWindow: touch.window === window,
                                 target: ancestors(touch.view, window: window),
                                 hitTest: ancestors(hit, window: window))
            append(Observation(sequence: sequence + 1, uptime: ProcessInfo.processInfo.systemUptime,
                               stage: stage, window: Window(window), intent: nil, ready: nil,
                               tab: nil, nativeSelectedTab: nil, route: nil, workspace: nil, modal: nil, scriptFailed: nil,
                               eventTimestamp: event.timestamp, touch: observed, navigationControls: controls))
        }
    }

    private func navigationControls(_ window: UIWindow) throws -> [NavigationControl] {
        guard let screen = window.windowScene?.screen else {
            throw HostError("Native navigation diagnostic window has no screen")
        }
        var remaining: [UIView] = [window]
        var result: [NavigationControl] = []
        var visited = 0
        while let view = remaining.popLast() {
            visited += 1
            guard visited <= 1024 else { throw HostError("Native navigation diagnostic view traversal exhausted") }
            if let identifier = view.accessibilityIdentifier, Self.navigationIdentifiers.contains(identifier) {
                guard result.count < 16 else { throw HostError("Native navigation diagnostic control bound exceeded") }
                let point = view.accessibilityActivationPoint
                let local = window.convert(point, from: screen.coordinateSpace)
                let hit = window.hitTest(local, with: nil)
                result.append(NavigationControl(identifier: identifier, view: View(view, window: window),
                    ancestors: identifier == "native-hosts" ? ancestors(view, window: window) : nil, accessibilityFrameInScreen: view.accessibilityFrame,
                    activationPointInScreen: point, activationPointInWindow: local,
                    activationHit: hit.map { View($0, window: window) },
                    activationHitNavigationAncestor: ancestors(hit, window: window)
                        .compactMap(\.control).first { Self.navigationIdentifiers.contains($0) }))
            }
            // The requested metadata is native chrome, never the retained page's view/content tree.
            if view.accessibilityIdentifier != "tyde-content" { remaining.append(contentsOf: view.subviews) }
        }
        return result.sorted { $0.identifier < $1.identifier }
    }

    private func ancestors(_ first: UIView?, window: UIWindow) -> [View] {
        var result: [View] = []
        var current = first
        while let view = current, result.count < 16 {
            result.append(View(view, window: window))
            current = view.superview
        }
        return result
    }

    private func append(_ observation: Observation) {
        guard !failed else { return }
        do {
            guard sequence < 128 else { throw HostError("Native navigation diagnostic trace exhausted") }
            var data = try JSONEncoder().encode(observation)
            guard data.count < 32 * 1024 else { throw HostError("Native navigation diagnostic record exceeded its bound") }
            data.append(0x0a)
            try file.write(contentsOf: data)
            sequence += 1
        } catch { fail(error) }
    }

    private func fail(_ error: Error) {
        failed = true
        let reason = error as NSError
        logger.error("Native navigation diagnostics failed domain=\(reason.domain, privacy: .public) code=\(reason.code); stop the case and preserve the partial trace")
    }
}
#endif
