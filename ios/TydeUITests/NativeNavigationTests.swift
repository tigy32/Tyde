import XCTest
import Darwin
import UIKit

final class NativeNavigationTests: XCTestCase {
    private var recordingEvidence = false

    private func capture(_ app: XCUIApplication, _ name: String) {
        let attachment = XCTAttachment(screenshot: app.screenshot())
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
        let hierarchy = XCTAttachment(string: app.debugDescription)
        hierarchy.name = "Private AX: \(name)"
        hierarchy.lifetime = .keepAlways
        add(hierarchy)
    }

    override func record(_ issue: XCTIssue) {
        let app = XCUIApplication()
        if !recordingEvidence && app.state == .runningForeground {
            recordingEvidence = true
            capture(app, "Failure boundary")
            recordingEvidence = false
        }
        super.record(issue)
    }

    private func require(_ element: XCUIElement, _ context: String,
                         file: StaticString = #filePath, line: UInt = #line) {
        // Preserve the actual destination even if the test watchdog ends the wait.
        capture(XCUIApplication(), "Before: \(context)")
        let failure = XCUIApplication().textViews["Tyde error"]
        let terminal = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            element.exists || failure.exists
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [terminal], timeout: 600), .completed, context, file: file, line: line)
        XCTAssertFalse(failure.exists, "A real native platform failure ended this operation; inspect its original capture", file: file, line: line)
        XCTAssertTrue(element.exists, context, file: file, line: line)
    }

    private func requireCompleteInput(_ element: XCUIElement, expected: String,
                                      file: StaticString = #filePath, line: UInt = #line) {
        // Native85 completed synthesis before UIKit's final observed insertions.
        // A returned typeText and app quiescence do not acknowledge complete editing.
        let failure = XCUIApplication().textViews["Tyde error"]
        let delivered = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            (element.value as? String) == expected || failure.exists
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [delivered], timeout: 600), .completed,
                       "The single typing action must establish the complete exact draft before continuing",
                       file: file, line: line)
        XCTAssertFalse(failure.exists, "A native platform failure ended input delivery; inspect its original capture",
                       file: file, line: line)
    }

    private func captureGeometryIsUsable(_ app: XCUIApplication, anchor: XCUIElement, keyboard: Bool) -> Bool {
        let window = app.windows.firstMatch
        let content = app.webViews["tyde-content"]
        guard app.state == .runningForeground, window.exists, content.exists,
              anchor.exists, anchor.isHittable else { return false }
        let bounds = window.frame
        guard bounds.width > 0, bounds.height > 0,
              content.frame.width > 0, content.frame.height > 0,
              anchor.frame.width > 0, anchor.frame.height > 0,
              bounds.contains(content.frame), bounds.contains(anchor.frame) else { return false }
        let allDone = app.buttons.matching(NSPredicate(format: "label == %@", "Done"))
        let webDone = app.webViews.buttons.matching(NSPredicate(format: "label == %@", "Done"))
        let assistants = app.otherElements.matching(identifier: "SystemInputAssistantView").allElementsBoundByIndex
        let input = app.keyboards.firstMatch
        if !keyboard {
            return !input.exists && allDone.count == webDone.count && !assistants.contains {
                $0.frame.width > 0 && $0.frame.height > 0 && bounds.intersects($0.frame)
            }
        }
        guard app.keyboards.count == 1, input.exists, anchor.isEnabled, anchor.hasFocus,
              webDone.count == 0, allDone.count <= 1 else { return false }
        let frame = input.frame
        guard frame.width > 0, frame.height > 0, bounds.contains(frame) else { return false }
        var occlusionTop = frame.minY
        // Actual31 iPad has no Done accessory; validate actual bars, not a phone-only presence rule.
        for bar in assistants + allDone.allElementsBoundByIndex {
            let rect = bar.frame
            guard rect.width > 0, rect.height > 0, bounds.contains(rect), rect.maxY <= frame.minY else { return false }
            occlusionTop = min(occlusionTop, rect.minY)
        }
        return anchor.frame.maxY <= occlusionTop
    }

    private func requireCaptureGeometry(_ app: XCUIApplication, anchor: XCUIElement, keyboard: Bool = false,
                                        file: StaticString = #filePath, line: UInt = #line) {
        capture(app, "Before capture geometry: keyboard=\(keyboard)")
        let usable = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            self.captureGeometryIsUsable(app, anchor: anchor, keyboard: keyboard)
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [usable], timeout: 600), .completed,
                       "Capture/typing requires a usable field, actual focus and on-screen window/content/keyboard/accessory geometry",
                       file: file, line: line)
    }

    private func captureSettled(_ app: XCUIApplication, _ name: String, anchor: XCUIElement, keyboard: Bool = false,
                                file: StaticString = #filePath, line: UInt = #line) {
        requireCaptureGeometry(app, anchor: anchor, keyboard: keyboard, file: file, line: line)
        capture(app, name)
    }

    private func nativeTab(_ app: XCUIApplication, _ destination: String,
                           file: StaticString = #filePath, line: UInt = #line) throws -> XCUIElement {
        let identifier = "native-tab-" + destination
        let matches = app.buttons.matching(identifier: identifier)
        require(matches.firstMatch, "The native \(destination) tab must exist", file: file, line: line)
        let candidates = matches.allElementsBoundByIndex
        // Actual30 iPad AX repeats UITabBarItem's ID on an owner's nested Button.
        // One owner must contain ALL other same-ID representations; unrelated duplicates fail.
        let owners = candidates.filter { candidate in
            candidate.descendants(matching: .button).matching(identifier: identifier).count == candidates.count - 1
        }
        let owner = try XCTUnwrap(owners.count == 1 ? owners.first : nil,
                                 "Native tab requires one containing owner, not unrelated duplicate controls (matches=\(candidates.count), owners=\(owners.count))",
                                 file: file, line: line)
        XCTAssertEqual(owner.label, destination.capitalized,
                       "The native owner must expose the intended human-visible destination", file: file, line: line)
        return owner
    }

    private func selectNativeTab(_ app: XCUIApplication, _ destination: String,
                                 file: StaticString = #filePath, line: UInt = #line) throws {
        let tab = try nativeTab(app, destination, file: file, line: line)
        if app.keyboards.firstMatch.exists {
            // Source15 observed phone Done; actual31 iPad exposes Hide keyboard inside its keyboard.
            capture(app, "Before native keyboard dismissal")
            let done = app.buttons.matching(NSPredicate(format: "label == %@", "Done"))
            let webDone = app.webViews.buttons.matching(NSPredicate(format: "label == %@", "Done"))
            let hide = app.keyboards.buttons.matching(NSPredicate(format: "label == %@", "Hide keyboard"))
            var dismissal: XCUIElement?
            let dismissible = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
                let candidates = done.allElementsBoundByIndex + hide.allElementsBoundByIndex
                guard app.keyboards.count == 1, webDone.count == 0, candidates.count == 1 else { return false }
                let candidate = candidates[0]
                let window = app.windows.firstMatch.frame
                let keyboard = app.keyboards.firstMatch.frame
                guard candidate.isEnabled, candidate.isHittable, window.contains(keyboard),
                      window.contains(candidate.frame), candidate.frame.width > 0, candidate.frame.height > 0 else { return false }
                let positioned = done.count == 1 ? candidate.frame.maxY <= keyboard.minY : keyboard.contains(candidate.frame)
                guard positioned else { return false }
                dismissal = candidate
                return true
            }, object: nil)
            XCTAssertEqual(XCTWaiter.wait(for: [dismissible], timeout: 600), .completed,
                           "Keyboard dismissal requires one actual actionable native Done or integrated Hide keyboard control", file: file, line: line)
            try XCTUnwrap(dismissal, "The observed keyboard dismissal action must be unambiguous", file: file, line: line).tap()
        }
        requireCaptureGeometry(app, anchor: tab, file: file, line: line)
        let actionable = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            !app.keyboards.firstMatch.exists && tab.isEnabled && tab.isHittable
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [actionable], timeout: 600), .completed,
                       "Keyboard dismissal and native tab actionability must precede activation", file: file, line: line)
        // Actual63's element.tap hit Agents although Home's advertised center hit Home.
        let touch = tab.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        let point = touch.screenPoint
        XCTAssertTrue(point.x.isFinite && point.y.isFinite && tab.frame.contains(point)
                      && app.windows.firstMatch.frame.contains(point),
                      "Physical tab activation must stay inside its actionable owner and window", file: file, line: line)
        let location = XCTAttachment(string: "destination=\(destination) physicalScreenPoint=\(point) ownerFrame=\(tab.frame)")
        location.name = "Native physical tab activation"
        location.lifetime = .keepAlways
        add(location)
        touch.tap()
        capture(app, "Before native \(destination) tab selection")
        let selected = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in tab.isSelected }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [selected], timeout: 600), .completed,
                       "The activated native tab must become selected before waiting for its content", file: file, line: line)
        XCTAssertTrue(tab.isSelected, "Native selection must reflect the retained client's presentation", file: file, line: line)
    }

    private func assertNativeNavigationExcluded(_ app: XCUIApplication,
                                                file: StaticString = #filePath, line: UInt = #line) {
        capture(app, "Before native modal navigation exclusion")
        let excluded = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            app.navigationBars.count == 0 && app.tabBars.count == 0
                && ["home", "agents", "sessions", "settings"].allSatisfy {
                    !app.buttons["native-tab-" + $0].exists
                }
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [excluded], timeout: 600), .completed,
                       "The native modal projection must exclude navigation before continuing", file: file, line: line)
        XCTAssertEqual(app.navigationBars.count, 0, "A web modal must exclude native navigation from accessibility", file: file, line: line)
        XCTAssertEqual(app.tabBars.count, 0, "A web modal must exclude global tabs from accessibility", file: file, line: line)
        for tab in ["home", "agents", "sessions", "settings"] {
            XCTAssertFalse(app.buttons["native-tab-" + tab].exists, "A web modal must own navigation focus", file: file, line: line)
        }
    }

    private func requireNoProjectChoice(_ app: XCUIApplication) -> (picker: XCUIElement, option: XCUIElement) {
        let dialogs = app.webViews.otherElements.matching(NSPredicate(format: "label BEGINSWITH %@", "Choose a project for this chat"))
        let picker = dialogs.firstMatch
        require(picker, "New chat must present the named project-choice dialog")
        XCTAssertEqual(dialogs.count, 1, "The project-choice dialog must be unambiguous")
        // Actual WebKit AX maps this listbox option to Other, not Button.
        let options = picker.otherElements.matching(NSPredicate(format: "label BEGINSWITH %@", "No project"))
        let option = options.firstMatch
        require(option, "New chat must offer the interactive No project choice")
        XCTAssertEqual(options.count, 1, "No project must identify exactly one choice in this dialog")
        XCTAssertTrue(option.isSelected, "The fresh unscoped draft must expose its selected No project choice")
        let actionable = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            option.isEnabled && option.isHittable
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [actionable], timeout: 600), .completed,
                       "The No project choice must be actionable before selection")
        assertNativeNavigationExcluded(app)
        captureSettled(app, "Project picker", anchor: option)
        return (picker, option)
    }

    private func orient(_ orientation: UIDeviceOrientation, app: XCUIApplication, anchor: XCUIElement,
                        file: StaticString = #filePath, line: UInt = #line) {
        XCUIDevice.shared.orientation = orientation
        let landscape = orientation.isLandscape
        let window = app.windows.firstMatch
        let content = app.webViews["tyde-content"]
        // A device/interface notification can precede both native and WebKit layout.
        let usable = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            guard window.exists, content.exists, anchor.exists else { return false }
            let bounds = window.frame
            let viewport = content.frame
            return bounds.width > 0 && bounds.height > 0
                && viewport.width > 0 && viewport.height > 0
                && (bounds.width > bounds.height) == landscape
                && (viewport.width > viewport.height) == landscape
                && bounds.contains(viewport) && anchor.isHittable
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [usable], timeout: 600), .completed,
                       "Rotation must expose the requested native/WebKit viewport and a usable control before capture or another rotation",
                       file: file, line: line)
    }

    private func settingsCaptures(_ app: XCUIApplication, _ pose: String) throws {
        try selectNativeTab(app, "settings")
        for section in ["Appearance", "Paired host", "Host", "Backend setup", "Subscription capacity",
                        "Custom agents", "MCP servers", "Steering", "Skills", "Host tools", "Native voice", "About"] {
            // Actual h2 AX is an Other heading group containing StaticText; CSS uppercases both.
            let groups = app.webViews.otherElements.matching(NSPredicate(format: "label ==[c] %@", section))
            let headings = groups.firstMatch.staticTexts.matching(NSPredicate(format: "label ==[c] %@", section))
            let heading = headings.firstMatch
            require(heading, "Every reused Settings section must remain accessible")
            XCTAssertEqual(groups.count, 1, "Settings headings must be distinct from their row values")
            XCTAssertEqual(headings.count, 1, "Each Settings section must expose one exact semantic label")
            // Scroll the existing heading into view, not navigate to another screen.
            heading.tap()
            XCTAssertTrue(heading.isHittable, "Settings section must be visible in the native viewport")
            XCTAssertEqual(heading.label.lowercased(), section.lowercased())
            captureSettled(app, "\(pose) Settings \(section)", anchor: heading)
            if section == "Paired host" {
                let forget = app.webViews.buttons["Forget paired host on this device"]
                require(forget, "The paired-host section must offer its existing confirmation action")
                forget.tap()
                let title = app.webViews.staticTexts["Forget host?"]
                require(title, "Forget must open its real confirmation without removing the pairing")
                let cancel = app.webViews.buttons["Cancel"]
                require(cancel, "The forget confirmation must offer Cancel")
                assertNativeNavigationExcluded(app)
                captureSettled(app, "\(pose) Forget confirmation", anchor: cancel)
                cancel.tap()
                capture(app, "Before forget confirmation dismissal")
                let dismissed = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
                    !title.exists && app.buttons.matching(identifier: "native-tab-settings").count > 0
                }, object: nil)
                XCTAssertEqual(XCTWaiter.wait(for: [dismissed], timeout: 600), .completed,
                               "Cancel must dismiss confirmation and restore native navigation without forgetting")
                let restored = try nativeTab(app, "settings")
                requireCaptureGeometry(app, anchor: restored)
                XCTAssertTrue(restored.isEnabled && restored.isHittable && restored.isSelected,
                              "Dismissal must restore the same actionable selected native Settings owner")
            }
            if section == "Host tools" {
                let open = app.webViews.buttons.matching(NSPredicate(format: "label == %@", "Open"))
                require(open.firstMatch, "Host tools must expose the existing filesystem Open action")
                XCTAssertEqual(open.count, 1, "The host browser action must be unambiguous")
                open.firstMatch.tap()
                let browser = app.webViews.descendants(matching: .any)
                    .matching(NSPredicate(format: "label == %@", "Host filesystem browser")).firstMatch
                require(browser, "Open must present the named real host filesystem dialog")
                let cancel = browser.buttons["Cancel"]
                require(cancel, "Real host browser must open")
                assertNativeNavigationExcluded(app)
                captureSettled(app, "\(pose) Host browser", anchor: cancel)
                cancel.tap()
                capture(app, "Before host browser dismissal")
                let dismissed = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
                    !browser.exists && app.buttons.matching(identifier: "native-tab-settings").count > 0
                }, object: nil)
                XCTAssertEqual(XCTWaiter.wait(for: [dismissed], timeout: 600), .completed,
                               "Cancel must close the host browser before returning to Settings")
                let restored = try nativeTab(app, "settings")
                requireCaptureGeometry(app, anchor: restored)
                XCTAssertTrue(restored.isEnabled && restored.isHittable && restored.isSelected,
                              "Closing the browser must restore the actionable selected native Settings owner")
            }
        }
    }

    func testNativeSecureStorage() {
        continueAfterFailure = false
        let app = XCUIApplication()
        app.launchEnvironment["TYDE_IOS_SECURE_STORAGE_CHECK"] = "1"
        app.launch()
        let passed = app.alerts["Native secure storage check passed"]
        let failed = app.alerts["Native secure storage check failed"]
        let settled = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            passed.exists || failed.exists
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [settled], timeout: 600), .completed,
                       "The real app-scoped Keychain check must complete or surface its platform error")
        capture(app, "Native secure storage boundary")
        XCTAssertTrue(passed.exists, "Native secure storage failed; inspect the actual error capture and platform diagnostics")
    }

    private enum ManualPairingEntry { case keyboard, systemPaste }

    func testNormalManualPairing() throws {
        try normalManualPairing(entry: .keyboard)
    }

    func testNormalSystemPastePairing() throws {
        try normalManualPairing(entry: .systemPaste)
    }

    private func normalManualPairing(entry: ManualPairingEntry) throws {
        continueAfterFailure = false
        let environment = ProcessInfo.processInfo.environment
        let forbidden = ["TYDE_IOS_PAIRING_URL", "TYDE_IOS_INPUT_CONTROL", "TYDE_IOS_INPUT_DIAGNOSTICS",
                         "TYDE_IOS_INPUT_EXPECTED", "TYDE_IOS_NATIVE_INPUT_CAPTURE", "TYDE_IOS_SECURE_STORAGE_CHECK",
                         "TYDE_IOS_NAVIGATION_DIAGNOSTICS"]
        XCTAssertTrue(forbidden.allSatisfy { environment[$0] == nil },
                      "Normal manual pairing must not use DEBUG input, URL injection or recorder modes")
        XCTAssertTrue(environment["TYDE_IOS_ANIMATION_DIAGNOSTICS"] == nil
                      || environment["TYDE_IOS_ANIMATION_DIAGNOSTICS"] == "1",
                      "Animation capture must be an explicit declared mode")
        let animationCapture = environment["TYDE_IOS_ANIMATION_DIAGNOSTICS"] == "1"
        XCTAssertTrue(environment["TYDE_IOS_ANIMATION_CONTROL"] == nil
                      || environment["TYDE_IOS_ANIMATION_CONTROL"] == "1",
                      "Plain manual control must be an explicit declared mode")
        let animationControl = environment["TYDE_IOS_ANIMATION_CONTROL"] == "1"
        XCTAssertFalse(animationCapture && animationControl, "The app and plain control are separate observations")
        XCTAssertFalse(entry == .systemPaste && (animationCapture || animationControl),
                       "System Paste is a distinct normal product journey, never a typing/control substitute")
        let inputPath = try XCTUnwrap(environment["TYDE_IOS_PAIRING_INPUT"], "Pairing requires the canonical late-offer channel")
        XCTAssertTrue(FileManager.default.isReadableFile(atPath: inputPath), "The private offer channel must exist before READY")
        let app = XCUIApplication()
        if animationControl {
            app.launchEnvironment["TYDE_IOS_INPUT_CONTROL"] = "plain-webkit"
            app.launchEnvironment["TYDE_IOS_ANIMATION_CONTROL"] = "1"
        }
        app.launch()
        func tap(_ element: XCUIElement, _ context: String) {
            require(element, context)
            let actionable = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
                element.isEnabled && element.isHittable
            }, object: nil)
            XCTAssertEqual(XCTWaiter.wait(for: [actionable], timeout: 600), .completed, context)
            element.tap()
        }
        let retainedOpenNames: [String]
        if animationControl {
            retainedOpenNames = []
            require(app.webViews.staticTexts["Plain WebKit input control"],
                    "The control must render its actual plain document, not the Tyde page")
        } else {
            try selectNativeTab(app, "home")
            let hosts = app.navigationBars.buttons.matching(identifier: "native-hosts")
            require(hosts.firstMatch, "The retained paired Home must expose its normal Hosts action")
            XCTAssertEqual(hosts.count, 1, "Hosts must be an unambiguous native action")
            tap(hosts.firstMatch, "Open the normal paired-host chooser")
            require(app.webViews.buttons["Pair another host"], "The normal chooser must finish rendering before recording its retained identities")
            retainedOpenNames = app.webViews.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Open "))
                .allElementsBoundByIndex.map { $0.label }
            XCTAssertFalse(retainedOpenNames.isEmpty, "This case requires its retained paired records, not a fresh or reset app")
            tap(app.webViews.buttons["Pair another host"], "Pair another host must use the normal scanner route")
            tap(app.webViews.buttons["Paste link"], "The ordinary manual link form must be reachable")
        }
        let fields = app.webViews.textViews.matching(NSPredicate(format: "label == %@", "Pairing link"))
        let field = fields.firstMatch
        require(field, "The normal form must expose its editable Pairing link field")
        XCTAssertEqual(fields.count, 1, "Pairing input must be unambiguous")
        let initial = try XCTUnwrap(field.value as? String, "The native field value must be observable")
        // A freshly opened WebKit editor may expose its placeholder as its raw empty value.
        XCTAssertTrue(initial.isEmpty || initial == field.placeholderValue,
                      "The new form must be empty, not a retained or prefilled pairing link")
        tap(field, "Focus the real empty pairing field before issuing an offer")
        requireCaptureGeometry(app, anchor: field, keyboard: true)
        // Actual78 exposes this public system action outside the WebView, not an app Paste button.
        let assistants = app.otherElements.matching(identifier: "SystemInputAssistantView")
        let pasteButtons = assistants.buttons.matching(identifier: "assistantPaste:forEvent:")
        let paste = pasteButtons.firstMatch
        func pasteGeometryIsUsable() -> Bool {
            guard captureGeometryIsUsable(app, anchor: field, keyboard: true),
                  assistants.count == 1, pasteButtons.count == 1,
                  paste.label == "Paste", paste.isHittable else { return false }
            let bar = assistants.firstMatch.frame
            return bar.width > 0 && bar.height > 0
                && app.windows.firstMatch.frame.contains(bar)
                && bar.contains(paste.frame) && paste.frame.width > 0 && paste.frame.height > 0
                && field.frame.maxY <= bar.minY && bar.maxY <= app.keyboards.firstMatch.frame.minY
        }
        if entry == .systemPaste {
            capture(app, "Before genuine system Paste readiness")
            let prepared = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
                pasteGeometryIsUsable()
            }, object: nil)
            XCTAssertEqual(XCTWaiter.wait(for: [prepared], timeout: 600), .completed,
                           "READY requires the actual native assistant Paste action and unobscured focused field")
        }
        let inputURL = URL(fileURLWithPath: inputPath)
        let receipt = inputURL.deletingLastPathComponent().appendingPathComponent("driver-ready.json")
        let readiness: [String: Any] = [
            "phase": animationControl ? "animation_control_ready" : (animationCapture ? "animation_capture_ready" : "driver_ready"),
            "entry": entry == .systemPaste ? "normal_manual_system_paste" : (animationControl ? "plain_webkit_manual_stop_after_input" : (animationCapture ? "normal_manual_keyboard_stop_after_input" : "normal_manual_keyboard")),
            "input": inputPath, "app_actionable": true, "field_actionable": true,
            "field_focused": true, "software_keyboard_visible": true, "input_delivery_proven": false,
            "driver_pid": ProcessInfo.processInfo.processIdentifier,
            "input_strategy": entry == .systemPaste ? "native_assistant_system_paste" : (animationCapture || animationControl ? "whole_string_control" : "xcui_per_character_native_value_ack"),
            "continuation_preloaded": true]
        try JSONSerialization.data(withJSONObject: readiness).write(to: receipt, options: .atomic)
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: receipt.path)
        print("\(entry == .systemPaste ? "TYDE_IOS_SYSTEM_PASTE_READY" : (animationControl ? "TYDE_IOS_ANIMATION_CONTROL_READY" : (animationCapture ? "TYDE_IOS_ANIMATION_CAPTURE_READY" : "TYDE_IOS_PAIRING_READY"))) \(inputPath)")
        fflush(stdout)
        let input = try FileHandle(forReadingFrom: inputURL)
        defer { try? input.close() }
        let data = try XCTUnwrap(input.readToEnd(), "The private offer channel closed without an offer")
        let payload = try JSONDecoder().decode(PairingInput.self, from: data)
        XCTAssertFalse(payload.pairing_link.isEmpty, "The issued pairing link must be present")
        let validOffer: () -> Bool
        if animationCapture || animationControl {
            validOffer = { true }
        } else {
            let expires = try XCTUnwrap(payload.expires_at_ms, "Normal pairing requires the unchanged issued expiry")
            validOffer = { Date().timeIntervalSince1970 * 1000 < Double(expires) }
        }
        guard validOffer() else {
            XCTFail("The issued offer expired before input; no input or reissue is permitted")
            return
        }
        guard captureGeometryIsUsable(app, anchor: field, keyboard: true) else {
            XCTFail("Prepared native field focus or keyboard/window geometry changed before input; no input is permitted")
            return
        }
        let platformFailure = app.textViews["Tyde error"]
        var ownedPasteboardChange: Int?
        defer {
            if let ownedPasteboardChange, UIPasteboard.general.changeCount == ownedPasteboardChange {
                UIPasteboard.general.items = []
            }
        }
        if entry == .systemPaste {
            XCTAssertEqual(payload.pairing_link.utf8.count, 421,
                           "This authorized product episode requires the complete issued421-byte fixture")
            guard pasteGeometryIsUsable(), validOffer(), !platformFailure.exists,
                  let preparedValue = field.value as? String,
                  preparedValue.isEmpty || preparedValue == field.placeholderValue else {
                XCTFail("The prepared system Paste destination or real offer changed; no input is permitted")
                return
            }
            let expires = try XCTUnwrap(payload.expires_at_ms, "System Paste requires the original offer expiry")
            // Prepare only this simulator's clipboard; insertion still requires the public OS action.
            UIPasteboard.general.setItems([[UIPasteboard.typeAutomatic: payload.pairing_link]], options: [
                .localOnly: true, .expirationDate: Date(timeIntervalSince1970: Double(expires) / 1000)
            ])
            ownedPasteboardChange = UIPasteboard.general.changeCount
            let actionable = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
                !validOffer() || platformFailure.exists || !field.exists || !field.hasFocus
                    || app.state != .runningForeground || paste.isEnabled
            }, object: nil)
            guard XCTWaiter.wait(for: [actionable], timeout: 600) == .completed,
                  validOffer(), !platformFailure.exists, pasteGeometryIsUsable(), paste.isEnabled,
                  UIPasteboard.general.changeCount == ownedPasteboardChange else {
                XCTFail("The actual system Paste action was not actionable before failure or expiry; no alternate input is permitted")
                return
            }
            print("TYDE_IOS_SYSTEM_PASTE_REQUEST count=1")
            fflush(stdout)
            paste.tap()
        } else if animationCapture || animationControl {
            // Keep the historical whole-string controls separate from ordinary user input.
            field.typeText(payload.pairing_link)
        } else {
            var previous = try XCTUnwrap(field.value as? String, "The focused native value must be observable")
            var prefix = ""
            var acknowledged = 0
            for character in payload.pairing_link {
                guard validOffer(), field.exists, field.hasFocus, !platformFailure.exists,
                      app.state == .runningForeground else {
                    XCTFail("The native input destination or real offer failed; no further input is permitted")
                    return
                }
                field.typeText(String(character))
                prefix.append(character)
                var changed: String?
                let acknowledgedInput = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
                    if !validOffer() || !field.exists || !field.hasFocus || platformFailure.exists || app.state != .runningForeground { return true }
                    guard let value = field.value as? String else { return true }
                    guard value != previous else { return false }
                    changed = value
                    return true
                }, object: nil)
                guard XCTWaiter.wait(for: [acknowledgedInput], timeout: 600) == .completed,
                      validOffer(), field.exists, field.hasFocus, !platformFailure.exists,
                      app.state == .runningForeground, let value = changed else {
                    XCTFail("The native character was not acknowledged before failure or real expiry; no retry is permitted")
                    return
                }
                let exactPrefix = Array(value.utf8) == Array(prefix.utf8)
                print("TYDE_IOS_NATIVE_PREFIX_ACK exact=\(exactPrefix) expected_utf8_count=\(prefix.utf8.count) actual_utf8_count=\(value.utf8.count)")
                fflush(stdout)
                guard exactPrefix else {
                    XCTFail("The changed native prefix differs from the issued input; no repair or further typing is permitted")
                    return
                }
                previous = value
                acknowledged += 1
            }
            print("TYDE_IOS_NATIVE_CHARACTER_INPUT_COMPLETE acknowledgements=\(acknowledged)")
            fflush(stdout)
        }
        let expectedCount = payload.pairing_link.utf8.count
        let returned = field.value as? String
        let returnedCount = returned?.utf8.count ?? -1
        print("\(entry == .systemPaste ? "TYDE_IOS_SYSTEM_PASTE_RETURN" : "TYDE_IOS_MANUAL_TYPING_RETURN") expected_utf8_count=\(expectedCount) actual_utf8_count=\(returnedCount)")
        fflush(stdout)
        // Actual37 A/B values kept growing after typeText returned, including the script-free control.
        // Await input size, not equality: a complete but wrong value must fail the one exact assertion.
        var completedValue = returnedCount >= expectedCount ? returned : nil
        var observedCount = returnedCount
        let delivered = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            if entry == .systemPaste && (!validOffer() || !field.exists || !field.hasFocus) { return true }
            if completedValue != nil { return true }
            if !field.exists || platformFailure.exists || app.state != .runningForeground { return true }
            let value = field.value as? String
            let count = value?.utf8.count ?? -1
            if count != observedCount {
                observedCount = count
                print("TYDE_IOS_MANUAL_INPUT_STATE actual_utf8_count=\(count)")
                fflush(stdout)
            }
            guard count >= expectedCount else { return false }
            completedValue = value
            return true
        }, object: nil)
        guard XCTWaiter.wait(for: [delivered], timeout: 600) == .completed else {
            XCTFail("The one input episode never reached a complete native input state; no further input is permitted")
            return
        }
        guard field.exists, !platformFailure.exists, app.state == .runningForeground else {
            XCTFail("The native input destination failed before completion; no further input is permitted")
            return
        }
        if entry == .systemPaste && (!validOffer() || !field.hasFocus) {
            XCTFail("System Paste did not complete before real expiry or focus loss; no further input is permitted")
            return
        }
        let actual = completedValue
        let exact = actual.map { Array($0.utf8) == Array(payload.pairing_link.utf8) } == true
        print("\(entry == .systemPaste ? "TYDE_IOS_SYSTEM_PASTE_COMPLETE" : "TYDE_IOS_MANUAL_INPUT_COMPLETE") exact=\(exact) expected_utf8_count=\(payload.pairing_link.utf8.count) actual_utf8_count=\(actual?.utf8.count ?? -1)")
        fflush(stdout)
        guard exact else {
            XCTFail("The native input episode did not produce exact full UTF8 equality; no further input is permitted")
            return
        }
        if animationCapture || animationControl {
            // Observe default typeText's completion wait without spending another live offer.
            print("TYDE_IOS_ANIMATION_CAPTURE_COMPLETE control=\(animationControl) exact=true; Continue=0 Pair=0")
            fflush(stdout)
            capture(app, "Private stop-after-input animation boundary")
            return
        }
        capture(app, "Private normal manual link after exact native equality")
        let beforeContinue = Set(app.webViews.staticTexts.allElementsBoundByIndex.map { $0.label })
        let continueButton = app.webViews.buttons["Continue"]
        guard validOffer() else {
            XCTFail("The issued offer expired before Continue; no reissue is permitted")
            return
        }
        tap(continueButton, "Continue must classify the normally entered link")
        let connect = app.webViews.buttons["Pair"]
        let failed = app.webViews.staticTexts["Pairing failed"]
        let classified = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            !validOffer() || connect.exists || failed.exists || platformFailure.exists
                // Checking… is an in-progress button label, never a terminal validation error.
                || (field.exists && continueButton.exists && continueButton.isEnabled
                    && app.webViews.staticTexts.allElementsBoundByIndex.contains { !beforeContinue.contains($0.label) && $0.label != "Checking…" })
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [classified], timeout: 600), .completed,
                       "Classification must present confirmation or its actual visible error, not wait past a terminal failure")
        capture(app, "Private normal host confirmation or classification failure")
        guard validOffer() else {
            XCTFail("The issued offer expired during classification; no reissue is permitted")
            return
        }
        guard connect.exists && !failed.exists && !platformFailure.exists else {
            XCTFail("The normally entered link did not reach host confirmation; inspect the private terminal capture")
            return
        }
        let linkURL = try XCTUnwrap(URLComponents(string: payload.pairing_link), "The issued HTTPS link must have an origin")
        XCTAssertEqual(linkURL.scheme, "https", "Normal direct pairing requires trusted HTTPS")
        var origin = URLComponents()
        origin.scheme = "https"
        origin.host = try XCTUnwrap(linkURL.host, "The issued link must identify its host")
        origin.port = linkURL.port == 443 ? nil : linkURL.port
        let expectedOrigin = try XCTUnwrap(origin.string, "The issued link must have a valid origin")
        require(app.webViews.staticTexts["Pair with this Tyde"], "The real direct-host confirmation must be visible")
        require(app.webViews.staticTexts[expectedOrigin], "Confirmation must identify the exact issued HTTPS origin")
        guard validOffer() else {
            XCTFail("The issued offer expired before Pair; no reissue is permitted")
            return
        }
        tap(connect, "Pair must be an enabled, hittable normal confirmation action")
        let connected = app.webViews.staticTexts.matching(NSPredicate(format: "label == %@", "Connected")).firstMatch
        let connectionError = app.webViews.buttons["Reconnect to this host"]
        let settled = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            connected.exists || failed.exists || platformFailure.exists || connectionError.exists
                || app.webViews.buttons["Re-pair"].exists || app.webViews.staticTexts["Update required"].exists
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [settled], timeout: 600), .completed,
                       "Pairing must reach authenticated client bootstrap or an actual terminal failure")
        capture(app, "Private normal pairing Home or terminal failure")
        XCTAssertTrue(connected.exists && !failed.exists && !platformFailure.exists && !connectionError.exists,
                      "Pairing requires the production Connected/bootstrap state, not merely tabs or successful input delivery")
        XCTAssertTrue(try nativeTab(app, "home").isSelected, "Successful Pair must open the newly paired host's Home")
        captureSettled(app, "Normal manual pairing Connected Home before selected-record correlation", anchor: connected)
        try selectNativeTab(app, "settings")
        let credentialLabel = app.webViews.staticTexts["Credential"]
        require(credentialLabel, "Settings must expose the active paired record's existing Credential row")
        let labels = app.webViews.staticTexts.allElementsBoundByIndex.map { $0.label }
        let credentialRows = labels.indices.filter { labels[$0] == "Credential" }
        XCTAssertEqual(credentialRows.count, 1, "The active paired-host credential row must be unambiguous")
        let index = try XCTUnwrap(credentialRows.first, "The selected record must expose its nonsecret fingerprint")
        XCTAssertTrue(labels.indices.contains(index + 1), "The credential label must have its rendered value")
        let selectedCredential = labels[index + 1]
        XCTAssertFalse(selectedCredential.isEmpty, "The selected pairing fingerprint must be present")
        XCTAssertFalse(retainedOpenNames.contains { $0.hasSuffix(", " + selectedCredential) },
                       "Pair must select the newly redeemed record, never any retained old host that was already Connected")
        capture(app, "Private selected-new-pairing credential for authoritative host correlation")
        try selectNativeTab(app, "home")
        tap(app.navigationBars.buttons["native-hosts"], "Read the newly selected record's normal chooser identity without changing selection")
        let newRows = app.webViews.buttons.matching(NSPredicate(format: "label BEGINSWITH %@ AND label ENDSWITH %@", "Open ", ", " + selectedCredential))
        require(newRows.firstMatch, "The newly selected pairing must also exist in the normal retained chooser")
        XCTAssertEqual(newRows.count, 1, "Only one chooser record may own the selected new credential")
        XCTAssertTrue(newRows.firstMatch.label.contains(", " + expectedOrigin + ", "),
                      "The selected new pairing must belong to the issued HTTPS origin, not another retained host")
        try selectNativeTab(app, "agents")
        require(app.navigationBars["Agents"], "The paired host's real Agents screen must render")
        captureSettled(app, "Normal manual pairing Agents for authoritative host/population correlation", anchor: app.navigationBars["Agents"])
    }

    private struct PairingInput: Decodable {
        let pairing_link: String
        let expires_at_ms: UInt64?
    }

    func testCapturePairedHostsAccessibility() throws {
        continueAfterFailure = false
        let app = XCUIApplication()
        app.launchEnvironment["TYDE_IOS_NAVIGATION_DIAGNOSTICS"] = "1"
        app.launch()
        let home = try nativeTab(app, "home")
        XCTAssertTrue(home.isSelected, "Read-only Hosts observation requires actual retained Home, not a prep tap")
        let hosts = app.navigationBars.buttons.matching(identifier: "native-hosts")
        require(hosts.firstMatch, "The native Home must expose its ordinary Hosts control")
        XCTAssertEqual(hosts.count, 1, "The observed native Hosts control must be unique")
        let target = hosts.element(boundBy: 0)
        let point = target.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)).screenPoint
        let properties = XCTAttachment(string: "public_isHittable=\(target.isHittable) enabled=\(target.isEnabled) frame=\(target.frame) public_center=\(point) window=\(app.windows.firstMatch.frame)")
        properties.name = "Private current public native Hosts properties, zero input"
        properties.lifetime = .keepAlways
        add(properties)
        capture(app, "Read-only native Hosts, zero input")
    }

    func testCapturePairedAgentsAccessibility() throws {
        continueAfterFailure = false
        let app = XCUIApplication()
        app.launch()
        let agents = try nativeTab(app, "agents")
        try selectNativeTab(app, "agents")
        let selected = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            agents.isSelected && app.webViews.buttons["Start a chat"].isHittable
        }, object: nil)
        let selectionResult = XCTWaiter.wait(for: [selected], timeout: 600)
        capture(app, "Agents accessibility diagnostic")
        let hierarchy = XCTAttachment(string: app.debugDescription)
        hierarchy.name = "Private Agents AX hierarchy"
        hierarchy.lifetime = .keepAlways
        add(hierarchy)
        let buttons = app.navigationBars.buttons.allElementsBoundByIndex.map { button in
            "identifier=\(button.identifier.debugDescription) label=\(button.label.debugDescription) frame=\(button.frame) enabled=\(button.isEnabled) hittable=\(button.isHittable)"
        }
        let properties = XCTAttachment(string: buttons.joined(separator: "\n"))
        properties.name = "Private native navigation button properties"
        properties.lifetime = .keepAlways
        add(properties)
        XCTAssertEqual(selectionResult, .completed,
                       "Capture requires the selected native Agents tab and the visible reused empty Agents screen")
    }

    func testCapturePairedProjectPickerAccessibility() throws {
        continueAfterFailure = false
        let app = XCUIApplication()
        app.launch()
        let agents = try nativeTab(app, "agents")
        try selectNativeTab(app, "agents")
        let newChat = app.navigationBars.buttons["New chat"]
        require(newChat, "Picker diagnostics must use the semantic native New chat action")
        XCTAssertEqual(newChat.label, "New chat")
        let actionable = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            agents.isSelected && app.navigationBars["Agents"].exists
                && newChat.isEnabled && newChat.isHittable
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [actionable], timeout: 600), .completed,
                       "The native New chat action must be usable before opening the picker")
        newChat.tap()
        capture(app, "Project picker after native action")
        let heading = app.webViews.descendants(matching: .any)
            .matching(NSPredicate(format: "label == %@", "Start a chat in")).firstMatch
        require(heading, "The reused project picker heading must appear before the diagnostic snapshot")
        capture(app, "Project picker accessibility diagnostic")
        let candidates = app.webViews.descendants(matching: .any)
            .matching(NSPredicate(format: "label CONTAINS %@", "No project"))
            .allElementsBoundByIndex.map { element in
                "type=\(element.elementType) identifier=\(element.identifier.debugDescription) label=\(element.label.debugDescription) frame=\(element.frame) enabled=\(element.isEnabled) hittable=\(element.isHittable) selected=\(element.isSelected)"
            }
        let properties = XCTAttachment(string: candidates.joined(separator: "\n"))
        properties.name = "Private No project AX candidates"
        properties.lifetime = .keepAlways
        add(properties)
    }

    func testPairedNativeNavigationRetainsComposer() throws {
        continueAfterFailure = false
        let app = XCUIApplication()
        let environment = ProcessInfo.processInfo.environment
        guard environment["TYDE_IOS_NATIVE_INPUT_CAPTURE"] == nil else {
            XCTFail("The retired debugger input-capture mode must not release typing")
            return
        }
        let inputDiagnostic = environment["TYDE_IOS_INPUT_DIAGNOSTICS"] == "1"
        let inputControl = environment["TYDE_IOS_INPUT_CONTROL"] ?? ""
        if !inputControl.isEmpty {
            guard inputControl == "plain-webkit", inputDiagnostic,
                  ["TYDE_IOS_PAIRING_URL", "TYDE_IOS_PAIRING_INPUT", "TYDE_IOS_NATIVE_INPUT_CAPTURE", "TYDE_IOS_SECURE_STORAGE_CHECK"]
                    .allSatisfy({ environment[$0] == nil }) else {
                XCTFail("Plain WebKit control requires the exclusive existing input-diagnostic route")
                return
            }
            app.launchEnvironment["TYDE_IOS_INPUT_CONTROL"] = inputControl
        }
        if inputDiagnostic {
            app.launchEnvironment["TYDE_IOS_INPUT_DIAGNOSTICS"] = "1"
            app.launchEnvironment["TYDE_IOS_INPUT_EXPECTED"] = "Retained native navigation draft"
        }
        app.launch()
        if inputControl == "plain-webkit" {
            let identity = XCTAttachment(string: "case=plain_webkit_control expected_utf16_units=32 typing_actions=1 product_acceptance=false")
            identity.name = "Input diagnostic case: plain_webkit_control"
            identity.lifetime = .keepAlways
            add(identity)
            let marker = app.webViews.staticTexts["Plain WebKit input control"]
            require(marker, "The actual document must identify the plain_webkit_control case")
            let composer = app.webViews.textViews["Message composer"]
            require(composer, "The plain WebKit control must expose its real editable textarea")
            orient(.portrait, app: app, anchor: composer)
            captureSettled(app, "plain_webkit_control before genuine focus", anchor: composer)
            XCTAssertTrue(composer.isEnabled && composer.isHittable,
                          "The real control must be editable and actionable before its native tap")
            composer.tap()
            requireCaptureGeometry(app, anchor: composer, keyboard: true)
            composer.typeText("Retained native navigation draft")
            requireCompleteInput(composer, expected: "Retained native navigation draft")
            let actual = composer.value as? String
            let exact = actual == "Retained native navigation draft"
            let length = String(describing: actual?.utf16.count)
            let observation = XCTAttachment(string: "scalarPresent=\(actual != nil) utf16Length=\(length) exactExpected=\(exact)")
            observation.name = "Content-free original input boundary (plain_webkit_control)"
            observation.lifetime = .keepAlways
            add(observation)
            capture(app, "plain_webkit_control after complete input acknowledgment")
            XCTAssertTrue(exact,
                          "The single original typing action must establish the complete exact draft before any menu interaction")
            return
        }
        let agents = try nativeTab(app, "agents")
        require(agents, "Navigation acceptance requires the same already-paired owned app")
        orient(.portrait, app: app, anchor: agents)
        captureSettled(app, "Portrait Home", anchor: agents)
        try selectNativeTab(app, "agents")
        let newChat = app.navigationBars.buttons["New chat"]
        require(newChat, "Native Agents must offer new chat")
        XCTAssertEqual(newChat.label, "New chat", "The native action must expose its product intent to accessibility")
        let actionable = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            agents.isSelected && app.navigationBars["Agents"].exists
                && newChat.isEnabled && newChat.isHittable
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [actionable], timeout: 600), .completed,
                       "The selected native Agents destination must expose an actionable New chat control")
        captureSettled(app, "Portrait Agents", anchor: newChat)
        newChat.tap()
        let choice = requireNoProjectChoice(app)
        choice.option.tap()
        let composer = app.webViews.textViews["Message composer"]
        require(composer, "New chat must retain the production composer")
        XCTAssertFalse(choice.picker.exists, "Selecting No project must close the project-choice dialog")
        captureSettled(app, "New chat options", anchor: composer)
        app.webViews.buttons["Change session settings for this chat"].tap()
        let done = app.webViews.buttons["Done"]
        require(done, "Draft session settings must open")
        assertNativeNavigationExcluded(app)
        captureSettled(app, "Draft session settings", anchor: done)
        done.tap()
        composer.tap()
        requireCaptureGeometry(app, anchor: composer, keyboard: true)
        composer.typeText("Retained native navigation draft")
        requireCompleteInput(composer, expected: "Retained native navigation draft")
        if inputDiagnostic {
            let actual = composer.value as? String
            let exact = actual == "Retained native navigation draft"
            let length = String(describing: actual?.utf16.count)
            let observation = XCTAttachment(string: "scalarPresent=\(actual != nil) utf16Length=\(length) exactExpected=\(exact)")
            observation.name = "Content-free original input boundary"
            observation.lifetime = .keepAlways
            add(observation)
            capture(app, "After complete input acknowledgment")
            XCTAssertTrue(exact,
                          "The single original typing action must establish the complete exact draft before any menu interaction")
            return
        }
        captureSettled(app, "Portrait composer keyboard", anchor: composer, keyboard: true)
        let sendGroups = app.webViews.otherElements.matching(NSPredicate(format: "label == %@", "Send actions"))
        let toggles = sendGroups.otherElements.matching(NSPredicate(format: "label == %@", "More send actions"))
        let sendMenu = toggles.firstMatch
        require(sendMenu, "The composer must expose its named send-actions popup trigger")
        XCTAssertEqual(sendGroups.count, 1, "The closed composer must have one send-actions group")
        XCTAssertEqual(toggles.count, 1, "The send-actions group must expose one named popup trigger")
        let popupActionable = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            sendMenu.isEnabled && sendMenu.isHittable
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [popupActionable], timeout: 600), .completed,
                       "The send-actions popup trigger must be actionable")
        sendMenu.tap()
        let photoAction = app.webViews.descendants(matching: .any)
            .matching(NSPredicate(format: "label == %@", "Add photos")).firstMatch
        require(photoAction, "Opening send actions must expose the real Add photos menu action")
        XCTAssertTrue(photoAction.isHittable, "The send menu contents must be visibly usable")
        captureSettled(app, "Composer split menu", anchor: photoAction)
        sendMenu.tap()
        capture(app, "Before send menu dismissal")
        let popupClosed = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            !photoAction.exists
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [popupClosed], timeout: 600), .completed,
                       "Closing the send menu must remove its actions without sending the draft")
        XCTAssertTrue((composer.value as? String) == "Retained native navigation draft", "Opening and closing send actions must retain the exact draft")
        try selectNativeTab(app, "sessions")
        let search = app.webViews.searchFields["Search sessions"]
        require(search, "Sessions must expose its existing filter")
        captureSettled(app, "Portrait Sessions", anchor: search)
        search.tap()
        requireCaptureGeometry(app, anchor: search, keyboard: true)
        search.typeText("Native acceptance filter")
        captureSettled(app, "Sessions filter keyboard", anchor: search, keyboard: true)
        try settingsCaptures(app, "Portrait")
        try selectNativeTab(app, "agents")
        XCTAssertTrue((composer.value as? String) == "Retained native navigation draft", "Native navigation must retain the exact draft")
        orient(.landscapeLeft, app: app, anchor: composer)
        XCTAssertTrue((composer.value as? String) == "Retained native navigation draft", "Rotation must retain the exact draft")
        captureSettled(app, "Landscape chat", anchor: composer)
        try selectNativeTab(app, "home")
        captureSettled(app, "Landscape Home", anchor: try nativeTab(app, "home"))
        try selectNativeTab(app, "sessions")
        XCTAssertTrue((search.value as? String) == "Native acceptance filter", "Native tabs and rotation must retain the exact filter")
        captureSettled(app, "Landscape Sessions", anchor: search)
        try settingsCaptures(app, "Landscape")
        try selectNativeTab(app, "agents")
        XCUIDevice.shared.press(.home)
        app.activate()
        capture(app, "Before foreground retained chat")
        let foreground = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            app.state == .runningForeground && agents.isSelected && composer.isHittable
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [foreground], timeout: 600), .completed,
                       "Foreground activation must restore the selected retained chat before checking its draft")
        XCTAssertTrue((composer.value as? String) == "Retained native navigation draft", "Background/foreground must retain the exact draft")
        orient(.portrait, app: app, anchor: composer)
        captureSettled(app, "Foreground portrait chat", anchor: composer)
        let backButtons = app.navigationBars.buttons.matching(NSPredicate(format: "label == %@", "Agents"))
        let back = backButtons.firstMatch
        require(back, "The system Back control must return to Agents")
        XCTAssertEqual(backButtons.count, 1, "The system Back destination must be unambiguous")
        XCTAssertEqual(back.label, "Agents", "Back must name the original Agents destination")
        XCTAssertTrue(back.isEnabled && back.isHittable)
        back.tap()
        require(newChat, "System Back must reveal Agents")
        captureSettled(app, "Native Agents after Back", anchor: newChat)
        newChat.tap()
        let secondChoice = requireNoProjectChoice(app)
        secondChoice.option.tap()
        require(composer, "A second No project selection must expose a fresh composer")
        XCTAssertFalse(secondChoice.picker.exists, "The second No project selection must close its dialog")
        XCTAssertEqual(composer.value as? String, "", "An explicit new conversation must not inherit the previous draft")
        composer.tap()
        requireCaptureGeometry(app, anchor: composer, keyboard: true)
        composer.typeText("Reply with one short sentence confirming this native client is connected.")
        let send = app.webViews.buttons["Send message"]
        require(send, "The fresh composer must expose its semantic Send message action")
        let sendReady = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in send.isEnabled && send.isHittable }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [sendReady], timeout: 600), .completed,
                       "The original Send message action must be enabled and hittable before activation")
        XCTAssertTrue(send.isEnabled, "Owned host must configure a real available backend for native chat acceptance")
        send.tap()
        let reply = app.webViews.descendants(matching: .any)
            .matching(NSPredicate(format: "label == %@", "Assistant message")).firstMatch
        require(reply, "A real backend response must arrive over the production connection")
        let more = app.navigationBars.buttons["More agent actions"]
        require(more, "A real agent must expose the native toolbar")
        captureSettled(app, "Real chat response", anchor: more)
        more.tap()
        let sessionSettings = app.buttons["Session settings"]
        require(sessionSettings, "The native agent menu must offer Session settings")
        captureSettled(app, "Native agent menu", anchor: sessionSettings)
        sessionSettings.tap()
        require(done, "Native toolbar must open reused session settings")
        assertNativeNavigationExcluded(app)
        captureSettled(app, "Agent session settings", anchor: done)
        done.tap()
        try selectNativeTab(app, "sessions")
        try selectNativeTab(app, "agents")
        XCTAssertTrue(reply.exists, "Returning through native tabs must retain the actual transcript")
        captureSettled(app, "Retained real transcript", anchor: more)
    }
}
