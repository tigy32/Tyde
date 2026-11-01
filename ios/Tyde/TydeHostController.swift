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
                  message.frameInfo.securityOrigin.protocol == "http",
                  message.frameInfo.securityOrigin.host == "127.0.0.1",
                  message.frameInfo.securityOrigin.port == 18736,
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
final class ContentController: UIViewController {
    var onVisible: ((ContentController) -> Void)?
    override func viewDidLoad() { super.viewDidLoad(); view.backgroundColor = .systemBackground }
    override func viewWillAppear(_ animated: Bool) { super.viewWillAppear(animated); onVisible?(self) }
}

@MainActor
final class TydeHostController: UIViewController, UITabBarControllerDelegate, UINavigationControllerDelegate, WKNavigationDelegate, WKUIDelegate {
    private let tabs = UITabBarController()
    private let entry = ContentController()
    private var activeContainer: UIViewController?
    private var failureMessages: [String] = []
    private var failureOverlay: UIView?
    private var failureText: UITextView?
    private let assets = AssetServer()
    private let platform = PlatformBridge()
    private var webView: WKWebView!
    private var roots: [NativeTab: ContentController] = [:]
    private var navigations: [NativeTab: UINavigationController] = [:]
    private var detailControllers: [NativeTab: ContentController] = [:]
    private var presentation: NativePresentation?
    private let logger = Logger(subsystem: "dev.tyde.mobile.nativeqa", category: "host")
    private var applyingPresentation = false
    private var ready = false
    private var lastRegularWidth: Bool?

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
        entry.onVisible = { [weak self] controller in self?.attach(to: controller) }
        overrideUserInterfaceStyle = .dark
        view.backgroundColor = UIColor(white: 0.071, alpha: 1)
        let configuration = WKWebViewConfiguration()
        var startupScripts: [WKUserScript] = []
        configuration.websiteDataStore = .default()
        configuration.allowsInlineMediaPlayback = true
        configuration.mediaTypesRequiringUserActionForPlayback = []
        configuration.defaultWebpagePreferences.allowsContentJavaScript = true
        platform.onPresentation = { [weak self] state in self?.render(state) }
        platform.presentingController = self
        for endpoint in [NativeEndpoint.presentation, .http, .http_cancel, .authenticate, .secret_read, .secret_write, .secret_delete] {
            configuration.userContentController.addScriptMessageHandler(platform, contentWorld: .page, name: endpoint.rawValue)
        }
        guard let bridgeURL = Bundle.main.url(forResource: "native-host", withExtension: "js"),
              let bridge = try? String(contentsOf: bridgeURL, encoding: .utf8) else {
            fail("Native bridge resource is missing")
            return
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
        webView = WKWebView(frame: .zero, configuration: configuration)
        #if DEBUG
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
            root.onVisible = { [weak self] controller in self?.attach(to: controller) }
            let navigation = UINavigationController(rootViewController: root)
            navigation.delegate = self
            navigation.tabBarItem = UITabBarItem(title: title, image: UIImage(systemName: image), selectedImage: nil)
            navigation.tabBarItem.accessibilityIdentifier = "native-tab-" + tab.rawValue
            roots[tab] = root
            navigations[tab] = navigation
        }
        tabs.viewControllers = [navigations[.home]!, navigations[.agents]!, navigations[.sessions]!, navigations[.settings]!]
        showContainer(entry)
        attach(to: entry)
        do {
            try assets.start(onReady: { [weak self] document in
                guard let self else { return }
                if !plainWebKitControl {
                    let bootstrap = "const tydeNativeBridgeVersion = \(nativeBridgeVersion); const tydeNativeHttpEndpoint = '\(NativeEndpoint.http.rawValue)'; const tydeNativeHttpCancelEndpoint = '\(NativeEndpoint.http_cancel.rawValue)'; const tydeNativeAssetOrigin = '\(document.assetOrigin.absoluteString)';\n" + bridge
                    configuration.userContentController.addUserScript(WKUserScript(source: bootstrap, injectionTime: .atDocumentStart, forMainFrameOnly: true))
                }
                for script in startupScripts { configuration.userContentController.addUserScript(script) }
                var data = document.data
                #if DEBUG
                if plainWebKitControl { data = Data(Self.plainWebKitDocument(pairing: plainManualControl).utf8) }
                #endif
                self.logger.notice("Native asset transport port=\(document.transportPort, privacy: .public); document origin retained")
                self.webView.loadSimulatedRequest(URLRequest(url: AssetServer.origin.appendingPathComponent("index.html")), response: document.response, responseData: data)
            }, onFailure: { [weak self] error in
                self?.fail("Bundled client listener failed: " + error.localizedDescription)
            })
        } catch { fail(error.localizedDescription) }
    }

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

    private func attach(to controller: ContentController) {
        guard webView != nil, webView.superview !== controller.view else { return }
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
        guard state.version == nativeBridgeVersion else { fail("Native presentation version mismatch"); return }
        applyingPresentation = true
        defer { applyingPresentation = false }
        logger.debug("Native projection tab=\(state.tab.rawValue, privacy: .public) route=\(state.route.rawValue, privacy: .public) workspace=\(state.workspace)")
        presentation = state
        ready = true
        #if DEBUG
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
            reportWidth()
            #if DEBUG
            observeApplied()
            #endif
            return
        }
        showContainer(tabs)
        // iPad's floating tabs are not descendants of the legacy tabBar view.
        tabs.setTabBarHidden(state.modal, animated: false)
        for navigation in navigations.values {
            navigation.tabBarItem.isEnabled = !state.modal
            navigation.setNavigationBarHidden(false, animated: false)
            navigation.navigationBar.isUserInteractionEnabled = !state.modal
            navigation.navigationBar.accessibilityElementsHidden = state.modal
            navigation.interactivePopGestureRecognizer?.isEnabled = !state.modal
        }
        guard let navigation = navigations[state.tab], let root = roots[state.tab] else { fail("Unknown native destination"); return }
        tabs.selectedViewController = navigation
        let content: ContentController
        if state.route != .root && state.workspace {
            if let existing = detailControllers[state.tab] { content = existing }
            else {
                content = ContentController()
                content.onVisible = { [weak self] controller in self?.attach(to: controller) }
                detailControllers[state.tab] = content
            }
            if navigation.topViewController !== content { navigation.setViewControllers([root, content], animated: false) }
        } else {
            content = root
            if navigation.topViewController !== root { navigation.setViewControllers([root], animated: false) }
        }
        content.navigationItem.title = state.title
        if let subtitle = state.subtitle {
            let heading = UIStackView()
            heading.axis = .vertical
            heading.alignment = .center
            let title = UILabel()
            title.text = state.title
            title.font = .preferredFont(forTextStyle: .headline)
            title.adjustsFontForContentSizeCategory = true
            let detail = UILabel()
            detail.text = subtitle
            detail.font = .preferredFont(forTextStyle: .caption1)
            detail.adjustsFontForContentSizeCategory = true
            detail.textColor = .secondaryLabel
            heading.addArrangedSubview(title)
            heading.addArrangedSubview(detail)
            content.navigationItem.titleView = heading
        } else { content.navigationItem.titleView = nil }
        if state.chat_actions {
            content.navigationItem.rightBarButtonItems = [moreButton()]
        } else if state.route == .root && state.tab == .agents {
            content.navigationItem.rightBarButtonItems = [newChatButton()]
        } else if state.route == .root && state.tab == .home {
            let hosts = UIBarButtonItem(title: "Hosts", primaryAction: UIAction(title: "Hosts") { [weak self] _ in self?.send(.hosts) })
            hosts.accessibilityLabel = "Hosts"
            hosts.accessibilityIdentifier = "native-hosts"
            content.navigationItem.rightBarButtonItems = [hosts]
        } else {
            content.navigationItem.rightBarButtonItems = []
        }
        content.navigationItem.rightBarButtonItems?.forEach { $0.isEnabled = !state.modal }
        attach(to: content)
        reportWidth()
        #if DEBUG
        observeApplied()
        #endif
    }

    private func newChatButton() -> UIBarButtonItem {
        let button = UIBarButtonItem(image: UIImage(systemName: "square.and.pencil"), primaryAction: UIAction(title: "New chat") { [weak self] _ in self?.send(.new_chat) })
        button.accessibilityLabel = "New chat"
        button.accessibilityIdentifier = "native-new-chat"
        return button
    }

    private func moreButton() -> UIBarButtonItem {
        let actions = [
            UIAction(title: "Rename agent", image: UIImage(systemName: "pencil")) { [weak self] _ in self?.send(.rename_agent) },
            UIAction(title: "Session settings", image: UIImage(systemName: "slider.horizontal.3")) { [weak self] _ in self?.send(.session_settings) },
            UIAction(title: "Close agent", image: UIImage(systemName: "xmark.circle"), attributes: .destructive) { [weak self] _ in self?.send(.close_agent) }
        ]
        let button = UIBarButtonItem(image: UIImage(systemName: "ellipsis"), menu: UIMenu(children: actions))
        button.accessibilityLabel = "More agent actions"
        return button
    }

    func send(_ intent: NativeIntent) {
        #if DEBUG
        navigationDiagnostics?.record(.intent_requested, window: view.window, state: presentation, ready: ready, intent: intent)
        #endif
        guard ready else { return }
        do {
            let json = String(decoding: try JSONEncoder().encode(intent), as: UTF8.self)
            webView.evaluateJavaScript("window.dispatchEvent(new CustomEvent('tyde-native-intent', {detail: \(json)}))") { [weak self] _, error in
                #if DEBUG
                if let self {
                    self.navigationDiagnostics?.record(.script_completed, window: self.view.window,
                        state: self.presentation, ready: self.ready, intent: intent, scriptFailed: error != nil)
                }
                #endif
                if error != nil { self?.fail("Native navigation could not reach the retained client") }
            }
        } catch { fail("Native intent could not be encoded") }
    }

    private func reportWidth() {
        guard ready else { return }
        let regular = traitCollection.horizontalSizeClass == .regular
        if lastRegularWidth != regular { lastRegularWidth = regular; send(regular ? .regular_width : .compact_width) }
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        reportWidth()
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

    func navigationController(_ navigationController: UINavigationController, didShow viewController: UIViewController, animated: Bool) {
        guard !applyingPresentation, let state = presentation, state.route != .root, state.workspace,
              navigationController === navigations[state.tab], viewController === roots[state.tab] else { return }
        send(.back)
    }

    func webView(_ webView: WKWebView, decidePolicyFor navigationAction: WKNavigationAction, decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
        guard let url = navigationAction.request.url else { decisionHandler(.cancel); fail("Invalid web navigation"); return }
        if url == AssetServer.origin.appendingPathComponent("index.html") && !ready {
            decisionHandler(.allow)
        } else if navigationAction.navigationType == .linkActivated, ["http", "https", "mailto"].contains(url.scheme), url.user == nil, url.password == nil {
            decisionHandler(.cancel)
            UIApplication.shared.open(url, options: [:]) { [weak self] opened in if !opened { self?.fail("External link could not open") } }
        } else {
            decisionHandler(.cancel)
            fail("Unexpected document navigation was blocked; the client was retained")
        }
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        webView.evaluateJavaScript("JSON.stringify({secureContext:window.isSecureContext,webCrypto:!!window.crypto?.subtle,indexedDB:!!window.indexedDB,webRTC:typeof RTCPeerConnection==='function',mediaCapture:typeof navigator.mediaDevices?.getUserMedia==='function',audioWorklet:typeof AudioWorkletNode==='function'})") { [weak self] value, error in
            if let value = value as? String, error == nil {
                self?.logger.notice("Native WebKit capabilities \(value, privacy: .public)")
            } else { self?.fail("Native platform capability diagnostics failed") }
        }
    }

    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) { fail("Bundled client navigation failed") }
    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) { fail("Bundled client could not load") }
    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        ready = false
        fail("The web content process ended. Reopen Tyde to reconnect; unsent in-memory changes may be lost.")
    }

    func webView(_ webView: WKWebView, requestMediaCapturePermissionFor origin: WKSecurityOrigin, initiatedByFrame frame: WKFrameInfo, type: WKMediaCaptureType, decisionHandler: @escaping (WKPermissionDecision) -> Void) {
        decisionHandler(origin.protocol == "http" && origin.host == "127.0.0.1" && origin.port == 18736 && frame.isMainFrame ? .prompt : .deny)
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
        let dismiss = UIButton(configuration: .borderedTinted(), primaryAction: UIAction(title: "Dismiss") { [weak self] _ in
            self?.failureOverlay?.removeFromSuperview()
            self?.failureOverlay = nil
            self?.failureText = nil
            self?.failureMessages.removeAll()
        })
        dismiss.setTitle("Dismiss", for: .normal)
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
