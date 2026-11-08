import UIKit
import WebKit
import OSLog

#if DEBUG
@MainActor
private final class NativeInputDiagnostics: NSObject, WKScriptMessageHandler {
    private struct Event: Codable {
        enum Phase: String, Codable {
            case ready, focusin, focusout, beforeinput, input, keydown, keypress, keyup
            case compositionstart, compositionend, setter, settled
            case prevent_default, dispatch_result, keyboard_complete
        }
        enum Input: String, Codable { case none, text, replacement, composition, deletion, other }
        enum KeyKind: String, Codable { case none, character, enter, escape, dead, unidentified, empty, other }
        let sequence: Int
        let node: Int
        let composer: Bool
        let phase: Phase
        let input: Input
        let setterBefore: Int?
        let setterBeforeExact: Bool?
        let setterBeforePrefix: Bool?
        let length: Int
        let exactExpected: Bool?
        let expectedPrefix: Bool?
        let documentTime: Double
        let dataLength: Int
        let selectionStart: Int?
        let selectionEnd: Int?
        let focused: Bool
        let connected: Bool
        let composing: Bool
        let trusted: Bool
        let keyKind: KeyKind
        let keyOrdinal: Int?
        let matchesExpected: Bool?
        let altKey: Bool
        let ctrlKey: Bool
        let metaKey: Bool
        let shiftKey: Bool
        let repeatKey: Bool
        let cancelable: Bool
        let defaultPrevented: Bool
        let eventPhase: Int
        let keydowns: Int
        let keyups: Int
        let expectedKeys: Int
    }
    private struct Observation: Encodable {
        let uptime: TimeInterval
        let timestampMs: TimeInterval
        let event: Event
    }
    private let file: FileHandle
    private var sequence = 0
    var onFailure: (() -> Void)?

    init(temporaryDirectory: URL) throws {
        let url = temporaryDirectory.appendingPathComponent("native-input-diagnostics-\(UUID().uuidString).jsonl")
        guard FileManager.default.createFile(atPath: url.path, contents: nil, attributes: [.posixPermissions: 0o600]) else {
            throw HostError("Native input diagnostics file could not be created")
        }
        file = try FileHandle(forWritingTo: url)
        super.init()
    }

    func userContentController(_ userContentController: WKUserContentController, didReceive message: WKScriptMessage) {
        do {
            guard message.frameInfo.isMainFrame,
                  ClientBootstrap.permits(message.frameInfo.securityOrigin),
                  let json = message.body as? String, json.utf8.count < 1024,
                  let data = json.data(using: .utf8) else {
                throw HostError("Untrusted native input diagnostic")
            }
            let event = try JSONDecoder().decode(Event.self, from: data)
            guard event.sequence == sequence + 1, event.sequence <= 512,
                  (0...512).contains(event.node),
                  event.composer == (event.node > 0),
                  (event.exactExpected != nil) == event.composer,
                  (event.expectedPrefix != nil) == event.composer,
                  event.documentTime.isFinite, event.documentTime >= 0,
                  (0...3).contains(event.eventPhase), (1...64).contains(event.expectedKeys),
                  [event.keyOrdinal, event.keydowns, event.keyups].compactMap({ $0 }).allSatisfy({ (0...512).contains($0) }),
                  [event.setterBefore, event.length, event.dataLength, event.selectionStart, event.selectionEnd]
                    .compactMap({ $0 }).allSatisfy({ (0...1_000_000).contains($0) }) else {
                throw HostError("Invalid or exhausted native input diagnostic sequence")
            }
            var line = try JSONEncoder().encode(Observation(uptime: ProcessInfo.processInfo.systemUptime,
                timestampMs: Date().timeIntervalSince1970 * 1000, event: event))
            line.append(0x0a)
            try file.write(contentsOf: line)
            sequence = event.sequence
        } catch { onFailure?() }
    }
}
#endif

@MainActor
private final class NativeBackAnimator: NSObject, UIViewControllerAnimatedTransitioning {
    private var animator: UIViewPropertyAnimator?
    private let onFailure: (String) -> Void

    init(onFailure: @escaping (String) -> Void) { self.onFailure = onFailure }

    func transitionDuration(using context: UIViewControllerContextTransitioning?) -> TimeInterval { 0.35 }

    func interruptibleAnimator(using context: UIViewControllerContextTransitioning) -> UIViewImplicitlyAnimating {
        if let animator { return animator }
        guard let source = context.view(forKey: .from), let target = context.view(forKey: .to) else {
            onFailure("Native Back transition views are unavailable")
            context.completeTransition(false)
            return UIViewPropertyAnimator()
        }
        let container = context.containerView
        target.frame = container.bounds
        target.transform = CGAffineTransform(translationX: -container.bounds.width * 0.3, y: 0)
        container.insertSubview(target, belowSubview: source)
        let animation = UIViewPropertyAnimator(duration: transitionDuration(using: context), curve: .easeOut) {
            source.transform = CGAffineTransform(translationX: container.bounds.width, y: 0)
            target.transform = .identity
        }
        animation.addCompletion { _ in
            source.transform = .identity
            target.transform = .identity
            context.completeTransition(!context.transitionWasCancelled)
        }
        animator = animation
        return animation
    }

    func animateTransition(using context: UIViewControllerContextTransitioning) {
        interruptibleAnimator(using: context).startAnimation()
    }
}

@MainActor
final class ContentController: UIViewController {
    var onVisible: ((ContentController) -> Void)?
    weak var connectedStatusView: UIView?
    override func viewDidLoad() { super.viewDidLoad(); view.backgroundColor = .clear }
    override func viewWillAppear(_ animated: Bool) { super.viewWillAppear(animated); onVisible?(self) }
}

@MainActor
final class TydeHostController: UIViewController, UITabBarControllerDelegate, UINavigationControllerDelegate, UIGestureRecognizerDelegate, WKNavigationDelegate, WKUIDelegate {
    private let tabs = UITabBarController()
    private let entry = ContentController()
    private var activeContainer: UIViewController?
    private var failureMessages: [String] = []
    private var failureOverlay: UIView?
    private var failureText: UITextView?
    private let platform = PlatformBridge()
    private var webView: WKWebView!
    private var roots: [NativeTab: ContentController] = [:]
    private var navigations: [NativeTab: UINavigationController] = [:]
    private var detailControllers: [NativeTab: ContentController] = [:]
    private var presentation: NativePresentation?
    private let logger = Logger(subsystem: "dev.tyde.mobile.nativeqa", category: "host")
    private var applyingPresentation = false
    private var backGestures: [NativeTab: UIScreenEdgePanGestureRecognizer] = [:]
    private var backInteraction: UIPercentDrivenInteractiveTransition?
    private var backSourceRevision: UInt64?
    private var backCommitRevision: UInt64?
    private var backDisplayLink: CADisplayLink?
    private weak var backSource: UIView?
    private var backEpoch: UInt64 = 0
    private var backScriptInFlight = false
    private var backPendingProgress: UInt32?
    private var backSamples = 0
    private var backMaximum: UInt32 = 0
    private enum DocumentState: String { case loading, live, terminated, recovering, failed }
    private var documentState = DocumentState.loading {
        didSet {
            guard documentState != oldValue else { return }
            logger.notice("Native document state=\(self.documentState.rawValue, privacy: .public)")
            if documentState != .live {
                stopBackSurface()
                backInteraction?.cancel()
                backInteraction = nil
                backCommitRevision = nil
                for gesture in backGestures.values { gesture.isEnabled = false }
                for content in Array(roots.values) + Array(detailControllers.values) {
                    content.connectedStatusView?.isHidden = true
                }
            }
        }
    }
    private var ready: Bool { documentState == .live }
    private var loadGeneration: UInt64 = 0
    private var loadDocument: (() -> Void)?
    private var documentNavigation: WKNavigation?
    private var lastLayout: NativeLayout?

    #if DEBUG
    var navigationDiagnostics: NativeNavigationDiagnostics?
    private let storageCheckRequested = ProcessInfo.processInfo.environment["TYDE_IOS_SECURE_STORAGE_CHECK"] == "1"
    private var storageCheckStarted = false

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        guard storageCheckRequested, !storageCheckStarted else { return }
        storageCheckStarted = true
        let alert: UIAlertController
        do {
            try platform.checkSecureStorage()
            alert = UIAlertController(title: "Native secure storage check passed", message: nil, preferredStyle: .alert)
        } catch {
            logger.error("Native secure storage check failed: \(error.localizedDescription, privacy: .public)")
            alert = UIAlertController(title: "Native secure storage check failed", message: error.localizedDescription, preferredStyle: .alert)
        }
        present(alert, animated: false)
    }
    #endif

    func start() {
        #if DEBUG
        NativeStartupDiagnostics.record(.host_start_entered)
        let environment = ProcessInfo.processInfo.environment
        let inputControl = environment["TYDE_IOS_INPUT_CONTROL"] ?? ""
        let plainWebKitControl = inputControl == "plain-webkit"
        let plainManualControl = environment["TYDE_IOS_ANIMATION_CONTROL"] == "1"
        if !inputControl.isEmpty {
            let declared = plainManualControl
                ? environment["TYDE_IOS_INPUT_DIAGNOSTICS"] == nil && environment["TYDE_IOS_INPUT_EXPECTED"] == nil
                : environment["TYDE_IOS_INPUT_DIAGNOSTICS"] == "1"
                    && environment["TYDE_IOS_INPUT_EXPECTED"] == "Retained native navigation draft"
            guard plainWebKitControl, declared, !storageCheckRequested,
                  ["TYDE_IOS_PAIRING_URL", "TYDE_IOS_PAIRING_INPUT", "TYDE_IOS_NATIVE_INPUT_CAPTURE",
                   "TYDE_IOS_NAVIGATION_DIAGNOSTICS"].allSatisfy({ environment[$0] == nil }) else {
                fail("Plain WebKit input control requires its exclusive declared diagnostic mode")
                return
            }
        } else if plainManualControl {
            fail("Plain manual input control requires the plain WebKit document")
            return
        }
        // The explicit native boundary check never starts networking or loads paired state.
        if storageCheckRequested { return }
        #else
        let plainWebKitControl = false
        #endif
        tabs.delegate = self
        entry.onVisible = { [weak self] controller in self?.contentWillAppear(controller) }
        overrideUserInterfaceStyle = .dark
        view.backgroundColor = UIColor(white: 0.071, alpha: 1)
        let configuration = WKWebViewConfiguration()
        var startupScripts: [WKUserScript] = []
        configuration.websiteDataStore = .default()
        configuration.allowsInlineMediaPlayback = true
        configuration.mediaTypesRequiringUserActionForPlayback = []
        configuration.defaultWebpagePreferences.allowsContentJavaScript = true
        configuration.limitsNavigationsToAppBoundDomains = true
        platform.onPresentation = { [weak self] state in self?.render(state) }
        platform.presentingController = self
        for endpoint in [NativeEndpoint.presentation, .http, .http_cancel, .authenticate, .secret_scope, .secret_read, .secret_write, .secret_delete] {
            configuration.userContentController.addScriptMessageHandler(platform, contentWorld: .page, name: endpoint.rawValue)
        }
        #if DEBUG
        if ProcessInfo.processInfo.environment["TYDE_IOS_INPUT_DIAGNOSTICS"] == "1" {
            do {
                guard let expected = ProcessInfo.processInfo.environment["TYDE_IOS_INPUT_EXPECTED"],
                      (1...64).contains(expected.utf16.count) else {
                    throw HostError("Native input diagnostics requires its declared test input")
                }
                let encoded = String(decoding: try JSONEncoder().encode(expected), as: UTF8.self)
                let diagnostics = try NativeInputDiagnostics(temporaryDirectory: FileManager.default.temporaryDirectory)
                diagnostics.onFailure = { [weak self] in self?.fail("Native input diagnostics failed; preserve the partial trace") }
                configuration.userContentController.add(diagnostics, name: "nativeInputDiagnostics")
                startupScripts.append(WKUserScript(source: "const tydeInputExpected = \(encoded);\n" + Self.inputDiagnosticScript, injectionTime: .atDocumentStart, forMainFrameOnly: true))
            } catch { fail("Native input diagnostics could not start"); return }
        }
        if let pairing = ProcessInfo.processInfo.environment["TYDE_IOS_PAIRING_URL"] {
            do {
                let encoded = String(decoding: try JSONEncoder().encode(pairing), as: UTF8.self)
                startupScripts.append(WKUserScript(source: "sessionStorage.setItem('tyde.pair.uri', \(encoded));", injectionTime: .atDocumentStart, forMainFrameOnly: true))
            } catch { fail("Native acceptance pairing input could not be encoded"); return }
        }
        #endif
        #if DEBUG
        NativeStartupDiagnostics.record(.wk_create_begin)
        #endif
        webView = WKWebView(frame: .zero, configuration: configuration)
        #if DEBUG
        NativeStartupDiagnostics.record(.wk_create_complete)
        webView.isInspectable = environment["TYDE_IOS_WEB_INSPECTOR"] == "1"
        #endif
        webView.navigationDelegate = self
        webView.uiDelegate = self
        webView.scrollView.contentInsetAdjustmentBehavior = .never
        webView.scrollView.isScrollEnabled = false
        webView.accessibilityIdentifier = "tyde-content"
        for (tab, title, image) in [(NativeTab.home, "Home", "house"), (.agents, "Agents", "play.fill"), (.sessions, "Sessions", "list.bullet"), (.settings, "Settings", "gearshape")] {
            let root = ContentController()
            root.title = title
            root.onVisible = { [weak self] controller in self?.contentWillAppear(controller) }
            let navigation = UINavigationController(rootViewController: root)
            navigation.delegate = self
            navigation.interactivePopGestureRecognizer?.isEnabled = false
            let edge = UIScreenEdgePanGestureRecognizer(target: self, action: #selector(interactiveBack(_:)))
            edge.edges = .left
            edge.delegate = self
            view.addGestureRecognizer(edge)
            backGestures[tab] = edge
            navigation.tabBarItem = UITabBarItem(title: title, image: UIImage(systemName: image), selectedImage: nil)
            navigation.tabBarItem.accessibilityIdentifier = "native-tab-" + tab.rawValue
            roots[tab] = root
            navigations[tab] = navigation
        }
        tabs.viewControllers = [navigations[.home]!, navigations[.agents]!, navigations[.sessions]!, navigations[.settings]!]
        showContainer(entry)
        attach(to: entry)
        #if DEBUG
        NativeStartupDiagnostics.record(.entry_attached)
        #endif
        var scriptsInstalled = false
        loadDocument = { [weak self] in
            guard let self else { return }
            self.loadGeneration += 1
            let generation = self.loadGeneration
            Task { @MainActor [weak self] in
                do {
                    #if DEBUG
                    NativeStartupDiagnostics.record(.resources_requested)
                    #endif
                    let (url, bridge) = try await Task.detached { try ClientBootstrap.load() }.value
                    guard let self, self.loadGeneration == generation else { return }
                    #if DEBUG
                    NativeStartupDiagnostics.record(.resources_loaded)
                    #endif
                    if !scriptsInstalled {
                        if !plainWebKitControl {
                            let chatDockOwner: String
                            if #available(iOS 27.1, *) {
                                chatDockOwner = "'\(NativeChatDock.owner(for: self.traitCollection).rawValue)'"
                            } else {
                                chatDockOwner = "null"
                            }
                            let bootstrap = "const tydeNativeChatDockOwner = \(chatDockOwner); const tydeNativeRootStatusOwner = '\(NativeRootStatusOwner.shell.rawValue)'; const tydeNativeChatHeaderOwner = '\(NativeChatHeaderOwner.client.rawValue)'; const tydeNativeBootstrapOrigin = '\(ClientBootstrap.origin.absoluteString)'; const tydeNativeBridgeVersion = \(nativeBridgeVersion); const tydeNativeBackSurfaceVersion = \(nativeBackSurfaceVersion); const tydeNativeBootstrapVersion = \(nativeBootstrapVersion); const tydeNativeShellRelease = '\(nativeShellRelease)'; const tydeNativeSecretScopeEndpoint = '\(NativeEndpoint.secret_scope.rawValue)'; const tydeNativeHttpEndpoint = '\(NativeEndpoint.http.rawValue)'; const tydeNativeHttpCancelEndpoint = '\(NativeEndpoint.http_cancel.rawValue)';\n" + bridge
                            configuration.userContentController.addUserScript(WKUserScript(source: bootstrap, injectionTime: .atDocumentStart, forMainFrameOnly: true))
                        }
                        for script in startupScripts { configuration.userContentController.addUserScript(script) }
                        scriptsInstalled = true
                    }
                    #if DEBUG
                    if plainWebKitControl {
                        self.webView.loadSimulatedRequest(URLRequest(url: url), responseHTML: Self.plainWebKitDocument(pairing: false))
                        return
                    }
                    #endif
                    #if DEBUG
                    NativeStartupDiagnostics.record(.wk_load_requested)
                    #endif
                    guard let navigation = self.webView.load(URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData)) else {
                        self.documentFailed("Native client navigation could not start")
                        return
                    }
                    self.documentNavigation = navigation
                    #if DEBUG
                    NativeStartupDiagnostics.record(.wk_load_created)
                    #endif
                } catch {
                    guard let self, self.loadGeneration == generation else { return }
                    #if DEBUG
                    NativeStartupDiagnostics.record(.resources_failed)
                    #endif
                    self.documentFailed("Native bootstrap could not start")
                }
            }
        }
        guard let loadDocument else { documentFailed("Native bootstrap is unavailable"); return }
        loadDocument()
        #if DEBUG
        for name in [UIResponder.keyboardWillChangeFrameNotification, UIResponder.keyboardDidChangeFrameNotification] {
            NotificationCenter.default.addObserver(self, selector: #selector(observeKeyboardFrame(_:)), name: name, object: nil)
        }
        #endif
    }

    func willEnterForeground() {
        if ready { send(.foreground) }
    }

    func didBecomeActive() {
        if documentState == .terminated { recoverDocument() }
    }

    private func recoverDocument() {
        guard documentState == .terminated || documentState == .failed else { return }
        clearFailure()
        documentState = .recovering
        platform.suspendClient()
        lastLayout = nil
        showContainer(entry)
        attach(to: entry)
        webView.isHidden = true
        guard let loadDocument else { documentFailed("Native recovery is unavailable"); return }
        loadDocument()
    }

    private func documentFailed(_ message: String) {
        documentState = .failed
        platform.suspendClient()
        fail(message)
    }

    private func clearFailure() {
        failureOverlay?.removeFromSuperview()
        failureOverlay = nil
        failureText = nil
        failureMessages.removeAll()
    }

    #if DEBUG
    @objc private func observeKeyboardFrame(_ notification: Notification) {
        guard navigationDiagnostics != nil else { return }
        let stage = notification.name == UIResponder.keyboardWillChangeFrameNotification ? "will-change" : "did-change"
        let frame = (notification.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? NSValue)?.cgRectValue
        let safe = view.safeAreaInsets
        let content = webView.superview
        let navigationHidden = navigations[presentation?.tab ?? .home]?.isNavigationBarHidden == true
        logger.debug("Native keyboard stage=\(stage, privacy: .public) document=\(self.documentState.rawValue, privacy: .public) hostWidth=\(self.view.bounds.width) hostHeight=\(self.view.bounds.height) safeTop=\(safe.top) safeBottom=\(safe.bottom) contentWidth=\(content?.bounds.width ?? 0) contentHeight=\(content?.bounds.height ?? 0) navHidden=\(navigationHidden) keyboardFramePresent=\(frame != nil) keyboardHeight=\(frame?.height ?? 0)")
    }
    #endif

    #if DEBUG
    private static func plainWebKitDocument(pairing: Bool) -> String {
        let editor = pairing
            ? #"<textarea aria-label="Pairing link" placeholder="https://host/tyde/…" rows="4"></textarea>"#
            : #"<textarea aria-label="Message composer" placeholder="Message..." enterkeyhint="enter" rows="1" data-mobile-test="chat-input"></textarea>"#
        return #"""
    <!DOCTYPE html>
    <html lang="en">
    <head>
        <meta charset="utf-8">
        <meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover, interactive-widget=resizes-content">
        <title>Plain WebKit input control</title>
        <style>
            body { margin: 0; padding: 16px; background: #121212; color: white; font: 16px -apple-system, sans-serif; }
            textarea { box-sizing: border-box; width: 100%; min-height: 48px; font: inherit; }
        </style>
    </head>
    <body>
        <p>Plain WebKit input control</p>
        \#(editor)
    </body>
    </html>
    """#
    }

    private static let inputDiagnosticScript = #"""
    (() => {
        "use strict";
        const descriptor = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value");
        const nodes = new WeakMap();
        const keyOrdinals = new WeakMap();
        const pendingKeys = [];
        const listeners = [];
        let nextNode = 0, sequence = 0;
        let keydowns = 0, keyups = 0;
        const composer = node => node instanceof HTMLTextAreaElement && node.matches('[data-mobile-test="chat-input"]');
        const read = node => descriptor.get.call(node);
        const record = (phase, node, event, setterBefore = null) => {
            const ownsInput = composer(node);
            if (ownsInput && !nodes.has(node)) nodes.set(node, ++nextNode);
            const value = ownsInput ? read(node) : null;
            const type = ownsInput ? event?.inputType || "" : "";
            const input = !type ? "none" : type === "insertText" ? "text"
                : type === "insertReplacementText" ? "replacement"
                : type.includes("Composition") ? "composition"
                : type.startsWith("delete") ? "deletion" : "other";
            const keyboard = event instanceof KeyboardEvent;
            const key = keyboard ? event.key : null;
            const keyKind = !keyboard ? "none" : key === "" ? "empty"
                : key === "Enter" ? "enter" : key === "Escape" ? "escape"
                : key === "Dead" ? "dead" : key === "Unidentified" ? "unidentified"
                : key.length === 1 ? "character" : "other";
            const keyOrdinal = event ? keyOrdinals.get(event) ?? null : null;
            window.webkit.messageHandlers.nativeInputDiagnostics.postMessage(JSON.stringify({
                sequence: ++sequence, node: ownsInput ? nodes.get(node) : 0, composer: ownsInput, phase, input,
                setterBefore: setterBefore?.length ?? null,
                setterBeforeExact: setterBefore === null ? null : setterBefore === tydeInputExpected,
                setterBeforePrefix: setterBefore === null ? null : tydeInputExpected.startsWith(setterBefore),
                length: value?.length ?? 0,
                exactExpected: value === null ? null : value === tydeInputExpected,
                expectedPrefix: value === null ? null : tydeInputExpected.startsWith(value),
                documentTime: performance.now(),
                dataLength: ownsInput && typeof event?.data === "string" ? event.data.length : 0,
                selectionStart: ownsInput ? node.selectionStart : null,
                selectionEnd: ownsInput ? node.selectionEnd : null,
                focused: !!node && document.activeElement === node, connected: !!node?.isConnected,
                composing: !!event?.isComposing, trusted: !!event?.isTrusted,
                keyKind, keyOrdinal,
                matchesExpected: !ownsInput || keyOrdinal === null ? null : keyOrdinal <= tydeInputExpected.length && key === tydeInputExpected[keyOrdinal - 1],
                altKey: !!event?.altKey, ctrlKey: !!event?.ctrlKey,
                metaKey: !!event?.metaKey, shiftKey: !!event?.shiftKey, repeatKey: !!event?.repeat,
                cancelable: !!event?.cancelable, defaultPrevented: !!event?.defaultPrevented,
                eventPhase: event?.eventPhase ?? 0, keydowns, keyups, expectedKeys: tydeInputExpected.length
            }));
        };
        const preventDefault = Event.prototype.preventDefault;
        Event.prototype.preventDefault = function(...args) {
            const result = Reflect.apply(preventDefault, this, args);
            if (composer(this.target) && (this.type === "keydown" || this.type === "keypress" || this.type === "beforeinput")) {
                record("prevent_default", this.target, this);
            }
            return result;
        };
        Object.defineProperty(HTMLTextAreaElement.prototype, "value", {
            ...descriptor,
            set(value) {
                const before = composer(this) ? read(this) : null;
                descriptor.set.call(this, value);
                if (composer(this)) record("setter", this, null, before);
            }
        });
        for (const phase of ["focusin", "focusout", "keydown", "keypress", "keyup", "beforeinput", "input", "compositionstart", "compositionend"]) {
            const observe = event => {
                if (!composer(event.target) && !["focusin", "focusout", "keydown", "keypress", "keyup"].includes(phase)) return;
                if (event.isTrusted && phase === "keydown") {
                    keyOrdinals.set(event, ++keydowns);
                    pendingKeys.push(event);
                }
                if (event.isTrusted && phase === "keyup") {
                    ++keyups;
                    // A native keyup occurs after prior keydown dispatch, unlike an early microtask.
                    for (const previous of pendingKeys.splice(0)) record("dispatch_result", previous.target, previous);
                }
                record(phase, event.target, event);
                if (phase === "input") queueMicrotask(() => record("settled", event.target, null));
                if (event.isTrusted && phase === "keyup" && keyups === tydeInputExpected.length) {
                    record("keyboard_complete", event.target, null);
                }
            };
            document.addEventListener(phase, observe, true);
            listeners.push([phase, observe]);
        }
        record("ready", null, null);
        return () => {
            for (const [phase, observe] of listeners) document.removeEventListener(phase, observe, true);
            Event.prototype.preventDefault = preventDefault;
            Object.defineProperty(HTMLTextAreaElement.prototype, "value", descriptor);
        };
    })();
    """#
    #endif

    private func showContainer(_ controller: UIViewController) {
        guard activeContainer !== controller else { return }
        if let previous = activeContainer {
            previous.willMove(toParent: nil)
            previous.view.removeFromSuperview()
            previous.removeFromParent()
        }
        addChild(controller)
        controller.view.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(controller.view)
        NSLayoutConstraint.activate([
            controller.view.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            controller.view.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            controller.view.topAnchor.constraint(equalTo: view.topAnchor),
            controller.view.bottomAnchor.constraint(equalTo: view.bottomAnchor)
        ])
        controller.didMove(toParent: self)
        activeContainer = controller
        if let failureOverlay { view.bringSubviewToFront(failureOverlay) }
    }

    private func contentWillAppear(_ controller: ContentController) {
        #if DEBUG
        logger.debug("Native back stage=will-appear root=\(self.roots.values.contains { $0 === controller }) route=\(self.presentation?.route.rawValue ?? "none", privacy: .public)")
        #endif
    }

    private func updateWorkspaceViewport() {
        guard ready, presentation?.workspace == true, webView.superview === view,
              let navigation = tabs.selectedViewController as? UINavigationController,
              let content = navigation.topViewController?.view else { return }
        webView.frame = content.convert(content.safeAreaLayoutGuide.layoutFrame, to: view)
    }

    private func attach(to controller: UIViewController) {
        guard webView != nil else { return }
        if controller is UINavigationController {
            if webView.superview !== view {
                logger.debug("Native back stage=attach owner=workspace route=\(self.presentation?.route.rawValue ?? "none", privacy: .public)")
                webView.removeFromSuperview()
                webView.translatesAutoresizingMaskIntoConstraints = true
                view.addSubview(webView)
            }
            updateWorkspaceViewport()
            if let failureOverlay { view.bringSubviewToFront(failureOverlay) }
            return
        }
        guard webView.superview !== controller.view else { return }
        logger.debug("Native back stage=attach owner=entry route=\(self.presentation?.route.rawValue ?? "none", privacy: .public)")
        webView.removeFromSuperview()
        webView.translatesAutoresizingMaskIntoConstraints = false
        controller.view.addSubview(webView)
        let safe = controller.view.safeAreaLayoutGuide
        NSLayoutConstraint.activate([
            webView.leadingAnchor.constraint(equalTo: safe.leadingAnchor), webView.trailingAnchor.constraint(equalTo: safe.trailingAnchor),
            webView.topAnchor.constraint(equalTo: safe.topAnchor), webView.bottomAnchor.constraint(equalTo: safe.bottomAnchor)
        ])
    }

    private func render(_ state: NativePresentation) {
        #if DEBUG
        NativeStartupDiagnostics.record(.presentation_received)
        #endif
        guard state.version == nativeBridgeVersion else { fail("Native presentation version mismatch"); return }
        if let version = state.back_surface_version, version != nativeBackSurfaceVersion {
            fail("Native Back surface contract mismatch"); return
        }
        if state.back_surface_version != nil && state.navigation_revision == nil {
            fail("Native Back surface revision is missing"); return
        }
        logger.debug("Native projection tab=\(state.tab.rawValue, privacy: .public) route=\(state.route.rawValue, privacy: .public) workspace=\(state.workspace)")
        guard documentState == .loading || documentState == .recovering || documentState == .live else { return }
        #if DEBUG
        if state.back_surface_version == nativeBackSurfaceVersion {
            NativeStartupDiagnostics.record(.back1_acknowledged)
            if state.navigation_revision != nil, state.back_ready_revision == state.navigation_revision,
               state.back_destination != nil {
                NativeStartupDiagnostics.record(.back1_ready)
            }
        }
        #endif
        presentation = state
        if let revision = backCommitRevision {
            guard state.navigation_revision != revision else { return }
            backCommitRevision = nil
        }
        // UIKit owns the in-flight gesture; heartbeat projections must not reset its stack.
        if state.client_ready, state.workspace,
           let navigation = tabs.selectedViewController as? UINavigationController,
           navigation.transitionCoordinator != nil {
            #if DEBUG
            logger.debug("Native back stage=projection-deferred route=\(state.route.rawValue, privacy: .public)")
            #endif
            return
        }
        applyPresentation(state)
    }

    private func applyPresentation(_ state: NativePresentation) {
        applyingPresentation = true
        defer { applyingPresentation = false }
        guard state.client_ready else {
            if documentState == .live { documentState = .loading }
            showContainer(entry)
            attach(to: entry)
            return
        }
        documentState = .live
        #if DEBUG
        NativeStartupDiagnostics.record(.client_ready)
        navigationDiagnostics?.record(.projection, window: view.window, state: state, ready: ready)
        let observeApplied = { [self] in
            let selected = navigations.first { $0.value === tabs.selectedViewController }?.key
            navigationDiagnostics?.record(.projection_applied, window: view.window, state: state,
                ready: ready, nativeSelectedTab: selected)
        }
        #endif
        overrideUserInterfaceStyle = state.theme == .dark ? .dark : .light
        webView.backgroundColor = state.theme == .dark ? UIColor(white: 0.071, alpha: 1) : UIColor(white: 0.98, alpha: 1)
        guard state.workspace else {
            showContainer(entry)
            attach(to: entry)
            reportLayout()
            #if DEBUG
            observeApplied()
            #endif
            return
        }
        showContainer(tabs)
        #if DEBUG
        NativeStartupDiagnostics.record(.workspace_attached)
        #endif
        // iPad's floating tabs are not descendants of the legacy tabBar view.
        tabs.setTabBarHidden(state.modal, animated: false)
        for navigation in navigations.values {
            navigation.tabBarItem.isEnabled = !state.modal
            navigation.setNavigationBarHidden(navigationBarHidden(for: state.route, state: state), animated: false)
            navigation.navigationBar.isUserInteractionEnabled = !state.modal && (state.route == .root || state.back_surface_version == nil || state.back_ready_revision == state.navigation_revision)
            navigation.navigationBar.accessibilityElementsHidden = state.modal
            navigation.interactivePopGestureRecognizer?.isEnabled = false
        }
        guard let navigation = navigations[state.tab], let root = roots[state.tab] else { fail("Unknown native destination"); return }
        tabs.selectedViewController = navigation
        for (tab, edge) in backGestures {
            edge.isEnabled = tab == state.tab && state.back_surface_version == nativeBackSurfaceVersion && state.back_destination != nil && state.back_ready_revision == state.navigation_revision && !state.modal && !state.sidebar_open
        }
        let content: ContentController
        if state.route != .root && state.workspace {
            if let existing = detailControllers[state.tab] { content = existing }
            else {
                content = ContentController()
                content.onVisible = { [weak self] controller in self?.contentWillAppear(controller) }
                detailControllers[state.tab] = content
            }
            if navigation.topViewController !== content { navigation.setViewControllers([root, content], animated: false) }
        } else {
            content = root
            if navigation.topViewController !== root { navigation.setViewControllers([root], animated: false) }
        }
        resetNavigationItem(content)
        if let destination = state.back_destination {
            resetNavigationItem(root)
            configureHeading(root, title: destination.title, subtitle: nil, chat: destination.route == .chat,
                connectedStatus: destination.connected_status)
            root.navigationItem.leftBarButtonItems = []
            if destination.route == .root { configureRootActions(root, tab: destination.tab) }
            else { root.navigationItem.rightBarButtonItems = [] }
            if destination.route == .chat, state.chat_dock_owner == .shell {
                guard state.chat_header_owner == .client, let dock = destination.chat_dock else {
                    fail("Native Back chat destination dock is missing"); return
                }
                if #available(iOS 27.1, *) {
                    NativeChatDock.apply(to: root, dock: dock, modal: state.modal,
                        menu: moreMenu(agentActions: dock.chat_actions, composerActions: true),
                        send: { [weak self] intent in self?.send(intent) })
                } else { fail("Native chat dock requires iOS 27.1 or later"); return }
            }
        }
        configureHeading(content, title: state.title, subtitle: state.subtitle, chat: state.route == .chat,
            connectedStatus: state.route != .chat && state.root_status_owner == .shell ? state.root_connected_status : nil)
        if state.route == .chat {
            content.navigationItem.rightBarButtonItems = [moreButton(agentActions: state.chat_actions, composerActions: state.chat_header_owner == .client)]
        } else if state.route == .root {
            configureRootActions(content, tab: state.tab)
        } else {
            content.navigationItem.rightBarButtonItems = []
        }
        content.navigationItem.hidesBackButton = state.sidebar_open
        content.navigationItem.leftItemsSupplementBackButton = !state.sidebar_open
        if state.sidebar_available {
            let sidebar = UIBarButtonItem(title: state.sidebar_open ? "Hide Agents" : "Show Agents",
                image: UIImage(systemName: "sidebar.left"),
                primaryAction: UIAction { [weak self] _ in self?.send(.toggle_sidebar) })
            sidebar.accessibilityLabel = state.sidebar_open ? "Hide Agents" : "Show Agents"
            sidebar.accessibilityIdentifier = "native-toggle-agents"
            sidebar.isEnabled = !state.modal
            content.navigationItem.leftBarButtonItems = [sidebar]
        } else {
            content.navigationItem.leftBarButtonItems = []
        }
        content.navigationItem.rightBarButtonItems?.forEach { $0.isEnabled = !state.modal }
        if state.route == .chat, state.chat_dock_owner == .shell {
            if #available(iOS 27.1, *) {
                do {
                    try NativeChatDock.apply(to: content, state: state,
                        menu: moreMenu(agentActions: state.chat_actions, composerActions: true),
                        send: { [weak self] intent in self?.send(intent) })
                } catch { fail(error.localizedDescription); return }
            } else {
                fail("Native chat dock requires iOS 27.1 or later")
                return
            }
        }
        attach(to: navigation)
        reportLayout()
        #if DEBUG
        observeApplied()
        #endif
    }

    private func navigationBarHidden(for route: NativeRoute, state: NativePresentation) -> Bool {
        route == .chat && state.chat_header_owner == .client && state.chat_dock_owner != .shell
    }

    private func resetNavigationItem(_ content: ContentController) {
        content.navigationItem.backAction = nil
        content.navigationItem.additionalOverflowItems = nil
        content.navigationItem.leadingItemGroups = []
        content.navigationItem.trailingItemGroups = []
    }

    private func configureHeading(_ content: ContentController, title headingTitle: String, subtitle: String?, chat: Bool,
                                  connectedStatus: NativeConnectedStatus?) {
        content.navigationItem.title = headingTitle
        content.connectedStatusView = nil
        if let status = connectedStatus {
            let heading = UIStackView()
            heading.axis = .horizontal
            heading.spacing = 8
            heading.alignment = .center
            let title = UILabel()
            title.text = headingTitle
            title.font = .preferredFont(forTextStyle: .headline)
            title.adjustsFontForContentSizeCategory = true
            title.lineBreakMode = .byTruncatingTail
            title.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
            let health = UIStackView()
            health.axis = .horizontal
            health.spacing = 5
            health.alignment = .center
            let dot = UIView()
            dot.backgroundColor = .systemGreen
            dot.layer.cornerRadius = 3
            dot.translatesAutoresizingMaskIntoConstraints = false
            NSLayoutConstraint.activate([dot.widthAnchor.constraint(equalToConstant: 6), dot.heightAnchor.constraint(equalToConstant: 6)])
            let latency = UILabel()
            latency.font = .preferredFont(forTextStyle: .subheadline)
            latency.adjustsFontForContentSizeCategory = true
            latency.textColor = .secondaryLabel
            if let milliseconds = status.round_trip_ms {
                latency.text = NativeLatencyText.format(milliseconds: milliseconds)
                latency.accessibilityLabel = "Connected, round trip \(milliseconds) milliseconds"
            } else {
                latency.text = "Checking…"
                latency.accessibilityLabel = "Connected, checking round-trip time"
            }
            latency.accessibilityIdentifier = "native-root-connection-status"
            health.addArrangedSubview(dot)
            health.addArrangedSubview(latency)
            health.setContentCompressionResistancePriority(.required, for: .horizontal)
            heading.addArrangedSubview(title)
            heading.addArrangedSubview(health)
            content.connectedStatusView = health
            content.navigationItem.titleView = heading
        } else if let subtitle = subtitle {
            let heading = UIStackView()
            heading.axis = chat ? .horizontal : .vertical
            heading.spacing = chat ? 8 : 0
            heading.alignment = .center
            let title = UILabel()
            title.text = headingTitle
            title.font = .preferredFont(forTextStyle: .headline)
            title.adjustsFontForContentSizeCategory = true
            if chat {
                title.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
            }
            let detail = UILabel()
            detail.text = subtitle
            detail.font = .preferredFont(forTextStyle: chat ? .subheadline : .caption1)
            detail.adjustsFontForContentSizeCategory = true
            detail.textColor = .secondaryLabel
            heading.addArrangedSubview(title)
            heading.addArrangedSubview(detail)
            content.navigationItem.titleView = heading
        } else { content.navigationItem.titleView = nil }
    }

    private func configureRootActions(_ content: ContentController, tab: NativeTab) {
        switch tab {
        case .agents: content.navigationItem.rightBarButtonItems = [newChatButton()]
        case .home:
            let hosts = UIBarButtonItem(title: "Hosts", primaryAction: UIAction(title: "Hosts") { [weak self] _ in self?.send(.hosts) })
            hosts.accessibilityLabel = "Hosts"
            hosts.accessibilityIdentifier = "native-hosts"
            content.navigationItem.rightBarButtonItems = [hosts]
        case .sessions, .settings: content.navigationItem.rightBarButtonItems = []
        }
    }

    private func newChatButton() -> UIBarButtonItem {
        let button = UIBarButtonItem(image: UIImage(systemName: "square.and.pencil"), primaryAction: UIAction(title: "New chat") { [weak self] _ in self?.send(.new_chat) })
        button.accessibilityLabel = "New chat"
        button.accessibilityIdentifier = "native-new-chat"
        return button
    }

    private func moreMenu(agentActions: Bool, composerActions: Bool) -> UIMenu {
        var actions: [UIAction] = []
        if composerActions {
            actions.append(UIAction(title: "Send actions", image: UIImage(systemName: "paperplane")) { [weak self] _ in self?.send(.send_actions) })
        }
        actions.append(UIAction(title: "Session settings", image: UIImage(systemName: "slider.horizontal.3")) { [weak self] _ in self?.send(.session_settings) })
        if agentActions {
            actions.append(UIAction(title: "Rename agent", image: UIImage(systemName: "pencil")) { [weak self] _ in self?.send(.rename_agent) })
            actions.append(UIAction(title: "Close agent", image: UIImage(systemName: "xmark.circle"), attributes: .destructive) { [weak self] _ in self?.send(.close_agent) })
        }
        return UIMenu(children: actions)
    }

    private func moreButton(agentActions: Bool, composerActions: Bool) -> UIBarButtonItem {
        let button = UIBarButtonItem(image: UIImage(systemName: "ellipsis"),
            menu: moreMenu(agentActions: agentActions, composerActions: composerActions))
        button.accessibilityLabel = "More agent actions"
        return button
    }

    func send(_ intent: NativeIntent) {
        #if DEBUG
        navigationDiagnostics?.record(.intent_requested, window: view.window, state: presentation, ready: ready, intent: intent)
        if intent == .back { logger.debug("Native back stage=intent-requested ready=\(self.ready)") }
        #endif
        guard ready else { return }
        do {
            let json = String(decoding: try JSONEncoder().encode(intent), as: UTF8.self)
            webView.evaluateJavaScript("window.dispatchEvent(new CustomEvent('tyde-native-intent', {detail: \(json)}))") { [weak self] _, error in
                #if DEBUG
                if let self {
                    if intent == .back { self.logger.debug("Native back stage=script-completed failed=\(error != nil)") }
                    self.navigationDiagnostics?.record(.script_completed, window: self.view.window,
                        state: self.presentation, ready: self.ready, intent: intent, scriptFailed: error != nil)
                }
                #endif
                if error != nil { self?.fail("Native navigation could not reach the retained client") }
            }
        } catch { fail("Native intent could not be encoded") }
    }

    private func reportLayout() {
        guard ready else { return }
        let size = view.bounds.size
        let layout: NativeLayout = traitCollection.horizontalSizeClass != .regular ? .compact
            : size.width > size.height ? .regular_landscape : .regular_portrait
        if #available(iOS 27.1, *), let declaredOwner = presentation?.chat_dock_owner {
            let owner = NativeChatDock.owner(for: traitCollection)
            if owner != declaredOwner {
                send(owner == .shell ? .chat_dock_shell : .chat_dock_client)
            }
        }
        guard lastLayout != layout else { return }
        lastLayout = layout
        logger.notice("Native layout=\(layout.rawValue, privacy: .public) width=\(size.width) height=\(size.height)")
        switch layout {
        case .compact: send(.compact_width)
        case .regular_portrait: send(.regular_width)
        case .regular_landscape: send(.regular_landscape)
        }
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        registerForTraitChanges([UITraitHorizontalSizeClass.self]) { (self: TydeHostController, _: UITraitCollection) in
            self.reportLayout()
        }
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        updateWorkspaceViewport()
        reportLayout()
        #if DEBUG
        navigationDiagnostics?.record(.layout, window: view.window, state: presentation, ready: ready)
        #endif
    }

    func tabBarController(_ tabBarController: UITabBarController, shouldSelect viewController: UIViewController) -> Bool {
        guard presentation?.workspace == true, presentation?.modal == false,
              let tab = navigations.first(where: { $0.value === viewController })?.key else { return false }
        switch tab { case .home: send(.home); case .agents: send(.agents); case .sessions: send(.sessions); case .settings: send(.settings) }
        return false
    }

    private func sendBackUpdate(_ update: NativeBackUpdate, completion: (() -> Void)? = nil) {
        guard ready else { return }
        let generation = loadGeneration
        do {
            let json = String(decoding: try JSONEncoder().encode(update), as: UTF8.self)
            webView.evaluateJavaScript("window.dispatchEvent(new CustomEvent('tyde-native-back', {detail: \(json)}))") { [weak self] _, error in
                guard let self, self.ready, self.loadGeneration == generation else { return }
                if error != nil { self.fail("Native Back could not reach the retained client"); return }
                completion?()
            }
        } catch { fail("Native Back progress could not be encoded") }
    }

    func gestureRecognizerShouldBegin(_ gestureRecognizer: UIGestureRecognizer) -> Bool {
        guard ready, let state = presentation, state.back_surface_version == nativeBackSurfaceVersion, !state.modal, !state.sidebar_open,
              state.back_destination != nil, state.back_ready_revision == state.navigation_revision, backCommitRevision == nil,
              gestureRecognizer === backGestures[state.tab],
              let navigation = navigations[state.tab] else { return false }
        return navigation.viewControllers.count > 1 && navigation.transitionCoordinator == nil
    }

    func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer,
                           shouldBeRequiredToFailBy otherGestureRecognizer: UIGestureRecognizer) -> Bool {
        backGestures.values.contains { $0 === gestureRecognizer }
            && otherGestureRecognizer.view?.isDescendant(of: webView) == true
    }

    @objc private func interactiveBack(_ gesture: UIScreenEdgePanGestureRecognizer) {
        guard let state = presentation, let navigation = navigations[state.tab] else { return }
        let width = navigation.view.bounds.width
        guard width > 0 else { return }
        let progress = min(1, max(0, gesture.translation(in: navigation.view).x / width))
        switch gesture.state {
        case .began:
            logger.debug("Native back stage=gesture-began route=\(state.route.rawValue, privacy: .public) headerHidden=\(navigation.isNavigationBarHidden) surfaceReady=\(state.back_ready_revision == state.navigation_revision)")
            let interaction = UIPercentDrivenInteractiveTransition()
            interaction.completionCurve = .easeOut
            backInteraction = interaction
            navigation.popViewController(animated: true)
        case .changed:
            backInteraction?.update(progress)
        case .ended:
            if progress > 0.5 || gesture.velocity(in: navigation.view).x > 500 {
                backInteraction?.finish()
            } else { backInteraction?.cancel() }
        case .cancelled, .failed: backInteraction?.cancel()
        default: break
        }
    }

    func navigationController(_ navigationController: UINavigationController,
                              animationControllerFor operation: UINavigationController.Operation,
                              from fromVC: UIViewController, to toVC: UIViewController) -> UIViewControllerAnimatedTransitioning? {
        operation == .pop && presentation?.back_surface_version == nativeBackSurfaceVersion ? NativeBackAnimator { [weak self] message in self?.fail(message) } : nil
    }

    func navigationController(_ navigationController: UINavigationController,
                              interactionControllerFor animationController: UIViewControllerAnimatedTransitioning) -> UIViewControllerInteractiveTransitioning? {
        backInteraction
    }

    @objc private func sampleBackSurface() {
        updateWorkspaceViewport()
        guard ready, backSourceRevision != nil, let source = backSource, let frame = source.layer.presentation()?.frame,
              let container = source.superview, container.bounds.width > 0 else { return }
        let fraction = min(1, max(0, frame.minX / container.bounds.width))
        let progress = UInt32((fraction * 1000).rounded())
        backSamples += 1
        backMaximum = max(backMaximum, progress)
        backPendingProgress = progress
        flushBackProgress()
    }

    private func flushBackProgress() {
        guard !backScriptInFlight, let progress = backPendingProgress,
              let revision = backSourceRevision else { return }
        backPendingProgress = nil
        backScriptInFlight = true
        let epoch = backEpoch
        sendBackUpdate(NativeBackUpdate(version: nativeBackSurfaceVersion, phase: .progress, source_revision: revision, progress: progress)) { [weak self] in
            guard let self, self.backEpoch == epoch else { return }
            self.backScriptInFlight = false
            self.flushBackProgress()
        }
    }

    private func stopBackSurface() {
        backDisplayLink?.invalidate()
        backDisplayLink = nil
        backSource = nil
        backSourceRevision = nil
        backEpoch += 1
        backPendingProgress = nil
        backScriptInFlight = false
        logger.debug("Native back stage=surface-ended samples=\(self.backSamples) maximum=\(self.backMaximum)")
    }

    func navigationController(_ navigationController: UINavigationController, willShow viewController: UIViewController, animated: Bool) {
        #if DEBUG
        logger.debug("Native back stage=will-show root=\(self.roots.values.contains { $0 === viewController }) interactive=\(navigationController.transitionCoordinator?.isInteractive == true) applying=\(self.applyingPresentation) route=\(self.presentation?.route.rawValue ?? "none", privacy: .public)")
        navigationController.transitionCoordinator?.notifyWhenInteractionChanges { [weak self] context in
            self?.logger.debug("Native back stage=interaction-ended cancelled=\(context.isCancelled)")
        }
        #endif
        if !applyingPresentation, ready, let state = presentation,
           navigationController === navigations[state.tab] {
            if viewController === roots[state.tab], let destination = state.back_destination {
                navigationController.setNavigationBarHidden(navigationBarHidden(for: destination.route, state: state), animated: animated)
            } else {
                navigationController.setNavigationBarHidden(navigationBarHidden(for: state.route, state: state), animated: animated)
            }
        }
        guard !applyingPresentation, ready, let state = presentation,
              navigationController === navigations[state.tab], state.back_surface_version == nativeBackSurfaceVersion,
              let revision = state.navigation_revision, state.back_destination != nil,
              viewController === roots[state.tab],
              let coordinator = navigationController.transitionCoordinator,
              let source = coordinator.viewController(forKey: .from)?.view else { return }
        stopBackSurface()
        backSamples = 0
        backMaximum = 0
        backSource = source
        backSourceRevision = revision
        sendBackUpdate(NativeBackUpdate(version: nativeBackSurfaceVersion, phase: .progress, source_revision: revision, progress: 0))
        let link = CADisplayLink(target: self, selector: #selector(sampleBackSurface))
        backDisplayLink = link
        link.add(to: .main, forMode: .common)
    }

    func navigationController(_ navigationController: UINavigationController, didShow viewController: UIViewController, animated: Bool) {
        #if DEBUG
        logger.debug("Native back stage=did-show root=\(self.roots.values.contains { $0 === viewController }) applying=\(self.applyingPresentation) route=\(self.presentation?.route.rawValue ?? "none", privacy: .public)")
        #endif
        guard !applyingPresentation, ready, let state = presentation else { return }
        let sourceRevision = backSourceRevision
        stopBackSurface()
        backInteraction = nil
        if navigationController === navigations[state.tab], state.route != .root,
           state.workspace, viewController === roots[state.tab] {
            if state.back_surface_version == nil {
                // ABI6 clients predate live surfaces; their one canonical Back intent remains supported.
                send(.back)
                return
            }
            guard let revision = sourceRevision else { fail("Native Back completed without its surface revision"); return }
            backCommitRevision = revision
            logger.debug("Native back stage=intent-requested ready=\(self.ready)")
            #if DEBUG
            navigationDiagnostics?.record(.intent_requested, window: view.window, state: presentation, ready: ready, intent: .back)
            #endif
            sendBackUpdate(NativeBackUpdate(version: nativeBackSurfaceVersion, phase: .complete, source_revision: revision, progress: 1000))
        } else {
            if let revision = sourceRevision {
                sendBackUpdate(NativeBackUpdate(version: nativeBackSurfaceVersion, phase: .cancel, source_revision: revision, progress: 0))
            }
            applyPresentation(state)
        }
    }

    func webView(_ webView: WKWebView, decidePolicyFor navigationAction: WKNavigationAction, decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
        guard let url = navigationAction.request.url else { decisionHandler(.cancel); fail("Invalid web navigation"); return }
        #if DEBUG
        let targetExists = navigationAction.targetFrame != nil
        let targetMain = navigationAction.targetFrame?.isMainFrame == true
        let sourceMain = navigationAction.sourceFrame.isMainFrame
        let entryEqual = url.absoluteString == ClientBootstrap.entry.absoluteString
        let schemeMatches = url.scheme == "https"
        let hostMatches = url.host == ClientBootstrap.origin.host
        let portMatches = (url.port ?? 443) == (ClientBootstrap.origin.port ?? 443)
        let noCredentials = url.user == nil && url.password == nil
        let pathMatches = url.path == "/tyde/"
        let entryPathMatches = url.path == ClientBootstrap.entry.path
        let encodedPathMatches = URLComponents(url: url, resolvingAgainstBaseURL: false)?.percentEncodedPath == "/tyde/"
        let queryMatches = url.query == "native=1"
        let canonicalEntryMatches = ClientBootstrap.permitsEntry(url)
        let blank = url.absoluteString == "about:blank"
        let empty = url.absoluteString.isEmpty
        let hasQuery = url.query != nil
        let hasFragment = url.fragment != nil
        logger.notice("Native navigation decision type=\(navigationAction.navigationType.rawValue) target=\(targetExists) targetMain=\(targetMain) sourceMain=\(sourceMain) entry=\(entryEqual) https=\(schemeMatches) host=\(hostMatches) port=\(portMatches) credentialsAbsent=\(noCredentials) path=\(pathMatches) entryPath=\(entryPathMatches) encodedPath=\(encodedPathMatches) query=\(queryMatches) canonicalEntry=\(canonicalEntryMatches) blank=\(blank) empty=\(empty) hasQuery=\(hasQuery) hasFragment=\(hasFragment)")
        #endif
        if navigationAction.navigationType == .linkActivated, ["http", "https", "mailto"].contains(url.scheme), url.user == nil, url.password == nil {
            decisionHandler(.cancel)
            UIApplication.shared.open(url, options: [:]) { [weak self] opened in if !opened { self?.fail("External link could not open") } }

        } else if ClientBootstrap.permitsEntry(url) {
            #if DEBUG
            NativeStartupDiagnostics.record(.wk_entry_admitted)
            #endif
            platform.suspendClient()
            decisionHandler(.allow)
        } else {
            decisionHandler(.cancel)
            fail("Unexpected document navigation was blocked; the client was retained")
        }
    }

    func webView(_ webView: WKWebView, didStartProvisionalNavigation navigation: WKNavigation!) {
        #if DEBUG
        NativeStartupDiagnostics.record(.wk_provisional_started)
        #endif
        documentNavigation = navigation
        if documentState == .live { documentState = .loading }
        platform.suspendClient()
    }

    func webView(_ webView: WKWebView, didCommit navigation: WKNavigation!) {
        guard navigation === documentNavigation else { return }
        guard webView.url?.host == ClientBootstrap.origin.host else { fail("Untrusted native document"); return }
        #if DEBUG
        NativeStartupDiagnostics.record(.wk_committed)
        #endif
        if documentState != .recovering { documentState = .loading }
        lastLayout = nil
        platform.beginBootstrap()
        webView.isHidden = false
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        guard navigation === documentNavigation else { return }
        #if DEBUG
        NativeStartupDiagnostics.record(.wk_finished)
        #endif
        webView.evaluateJavaScript("JSON.stringify({secureContext:window.isSecureContext,webCrypto:!!window.crypto?.subtle,indexedDB:!!window.indexedDB,webRTC:typeof RTCPeerConnection==='function',mediaCapture:typeof navigator.mediaDevices?.getUserMedia==='function',audioWorklet:typeof AudioWorkletNode==='function'})") { [weak self] value, error in
            guard let self, self.documentNavigation === navigation else { return }
            if let value = value as? String, error == nil {
                self.logger.notice("Native WebKit capabilities \(value, privacy: .public)")
            } else { self.fail("Native platform capability diagnostics failed") }
        }
    }

    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
        guard navigation === documentNavigation else { return }
        #if DEBUG
        NativeStartupDiagnostics.record(.wk_navigation_failed)
        #endif
        documentFailed("Native client navigation failed")
    }
    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
        guard navigation === documentNavigation else { return }
        #if DEBUG
        NativeStartupDiagnostics.record(.wk_provisional_failed)
        #endif
        documentFailed("Native client could not load")
    }
    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        #if DEBUG
        NativeStartupDiagnostics.record(.wk_process_terminated)
        #endif
        let interruptedRecovery = documentState == .recovering
        loadGeneration += 1
        documentState = .terminated
        platform.suspendClient()
        webView.isHidden = true
        showContainer(entry)
        attach(to: entry)
        if interruptedRecovery {
            documentFailed("The web content process ended during recovery. Retry to reconnect.")
        } else if view.window?.windowScene?.activationState == .foregroundActive {
            recoverDocument()
        }
    }

    func webView(_ webView: WKWebView, requestMediaCapturePermissionFor origin: WKSecurityOrigin, initiatedByFrame frame: WKFrameInfo, type: WKMediaCaptureType, decisionHandler: @escaping (WKPermissionDecision) -> Void) {
        decisionHandler(ClientBootstrap.permits(origin) && frame.isMainFrame ? .prompt : .deny)
    }

    private func fail(_ message: String) {
        logger.error("Native platform failure: \(message, privacy: .public)")
        if !failureMessages.contains(message) { failureMessages.append(message) }
        if let failureText {
            failureText.text = failureMessages.joined(separator: "\n\n")
            failureText.scrollRangeToVisible(NSRange(location: (failureText.text as NSString).length, length: 0))
            return
        }
        let overlay = UIView()
        overlay.translatesAutoresizingMaskIntoConstraints = false
        overlay.backgroundColor = UIColor.black.withAlphaComponent(0.35)
        overlay.accessibilityViewIsModal = true
        let text = UITextView()
        text.isEditable = false
        text.backgroundColor = .clear
        text.font = .preferredFont(forTextStyle: .body)
        text.adjustsFontForContentSizeCategory = true
        text.textColor = .label
        text.text = failureMessages.joined(separator: "\n\n")
        text.accessibilityLabel = "Tyde error"
        let retry = documentState == .failed
        let title = retry ? "Retry" : "Dismiss"
        let dismiss = UIButton(configuration: .borderedTinted(), primaryAction: UIAction(title: title) { [weak self] _ in
            guard let self else { return }
            if retry { self.recoverDocument() } else { self.clearFailure() }
        })
        dismiss.setTitle(title, for: .normal)
        let panel = UIStackView(arrangedSubviews: [text, dismiss])
        panel.translatesAutoresizingMaskIntoConstraints = false
        panel.axis = .vertical
        panel.spacing = 12
        panel.backgroundColor = .secondarySystemGroupedBackground
        panel.layer.cornerRadius = 16
        panel.layoutMargins = UIEdgeInsets(top: 16, left: 16, bottom: 16, right: 16)
        panel.isLayoutMarginsRelativeArrangement = true
        overlay.addSubview(panel)
        view.addSubview(overlay)
        let width = panel.widthAnchor.constraint(equalToConstant: 480)
        width.priority = .defaultHigh
        let textHeight = text.heightAnchor.constraint(equalToConstant: 200)
        textHeight.priority = .defaultHigh
        let safe = overlay.safeAreaLayoutGuide
        NSLayoutConstraint.activate([
            overlay.leadingAnchor.constraint(equalTo: view.leadingAnchor), overlay.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            overlay.topAnchor.constraint(equalTo: view.topAnchor), overlay.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            panel.centerXAnchor.constraint(equalTo: safe.centerXAnchor), panel.centerYAnchor.constraint(equalTo: safe.centerYAnchor),
            panel.widthAnchor.constraint(lessThanOrEqualTo: safe.widthAnchor, constant: -32),
            panel.heightAnchor.constraint(lessThanOrEqualTo: safe.heightAnchor, constant: -32), width, textHeight,
            dismiss.heightAnchor.constraint(greaterThanOrEqualToConstant: 44)
        ])
        failureOverlay = overlay
        failureText = text
        UIAccessibility.post(notification: .layoutChanged, argument: text)
    }
}
