import UIKit

enum NativeLatencyText {
    /// Fixed-width readout: at most three digits plus a unit, for any `UInt64`.
    static func format(milliseconds ms: UInt64) -> String {
        if ms < 1_000 { return "\(ms)ms" }
        if ms < 9_950 {
            let tenths = (ms + 50) / 100
            return "\(tenths / 10).\(tenths % 10)s"
        }
        if ms < 999_500 { return "\((ms + 500) / 1_000)s" }
        if ms < 3_570_000 { return "\((ms + 30_000) / 60_000)m" }
        let hours = ms / 3_600_000 + (ms % 3_600_000 >= 1_800_000 ? 1 : 0)
        return "\(min(hours, 999))h"
    }
}

@MainActor
final class NativeConnectionDockView: UILabel {
    override init(frame: CGRect) {
        super.init(frame: frame)
        configure()
    }

    required init?(coder: NSCoder) {
        super.init(coder: coder)
        configure()
    }

    override var intrinsicContentSize: CGSize { CGSize(width: 40, height: 40) }

    private func configure() {
        // The slot is fixed at 40pt, so Dynamic Type stops where five characters still fit.
        font = UIFontMetrics(forTextStyle: .caption2).scaledFont(
            for: .monospacedDigitSystemFont(ofSize: 11, weight: .regular), maximumPointSize: 12)
        adjustsFontForContentSizeCategory = true
        textColor = .secondaryLabel
        textAlignment = .center
        numberOfLines = 2
        lineBreakMode = .byClipping
        isUserInteractionEnabled = false
        isAccessibilityElement = true
        accessibilityIdentifier = "native-connection-status"
        setContentCompressionResistancePriority(.required, for: .horizontal)
        setContentCompressionResistancePriority(.required, for: .vertical)
        isHidden = true
    }

    func render(_ status: NativeChatConnectionStatus?) {
        guard let status else {
            text = nil
            accessibilityLabel = nil
            isHidden = true
            return
        }

        let label: String
        let dotColor: UIColor
        switch status.phase {
        case .pending:
            label = ""
            dotColor = .systemYellow
            accessibilityLabel = "Reconnecting"
        case .connected:
            dotColor = .systemGreen
            if let milliseconds = status.round_trip_ms {
                label = NativeLatencyText.format(milliseconds: milliseconds)
                accessibilityLabel = "Connected, round trip \(milliseconds) milliseconds"
            } else {
                label = "—ms"
                accessibilityLabel = "Connected, checking round-trip time"
            }
        }
        let display = NSMutableAttributedString(string: "●", attributes: [.foregroundColor: dotColor])
        if !label.isEmpty {
            display.append(NSAttributedString(string: "\n" + label, attributes: [.foregroundColor: UIColor.secondaryLabel]))
        }
        attributedText = display
        isHidden = false
    }

    /// Holds the readout's row, empty, using the same metrics as a live readout.
    func reserveSlot() {
        render(nil)
        isHidden = false
        isAccessibilityElement = false
    }
}

@available(iOS 27.1, *)
@MainActor
enum NativeActivityRailChrome {
    static func group(_ item: UIBarButtonItem) -> UIBarButtonItemGroup {
        item.axisBehavior = .verticalPreferred
        item.sharesBackground = false
        return UIBarButtonItemGroup(barButtonItems: [item], representativeItem: nil)
    }

    static func status(on content: ContentController, status: NativeChatConnectionStatus?,
                       identifier: String? = nil) -> UIBarButtonItemGroup {
        let health = NativeConnectionDockView(frame: .zero)
        if let status {
            health.render(status)
            if let identifier { health.accessibilityIdentifier = identifier }
            content.connectedStatusView = health
        } else {
            // Reserve the first row so the actions don't move when the status disappears.
            health.reserveSlot()
        }
        let item = UIBarButtonItem(customView: health)
        item.hidesSharedBackground = true
        return group(item)
    }

    static func back(label: String, enabled: Bool, send: @escaping (NativeIntent) -> Void) -> UIBarButtonItemGroup {
        let back = UIBarButtonItem(title: label, image: UIImage(systemName: "chevron.backward"),
            primaryAction: UIAction(title: label) { _ in send(.back) })
        back.accessibilityLabel = label
        back.accessibilityIdentifier = "native-rail-back"
        back.isEnabled = enabled
        return group(back)
    }
}

@available(iOS 27.1, *)
@MainActor
enum NativeChatDock {
    static func owner(for traits: UITraitCollection) -> NativeChatDockOwner {
        traits.verticalBarEdge == .unspecified ? .client : .shell
    }

    static func apply(to content: ContentController, state: NativePresentation, menu: UIMenu, newChat: UIBarButtonItem? = nil,
                      send: @escaping (NativeIntent) -> Void) throws {
        guard state.chat_header_owner == .client, let backLabel = state.chat_back_label else {
            throw HostError("Native chat dock requires its chat-local heading and Back label")
        }
        let dock = NativeBackChatDock(back_label: backLabel, connection_status: state.chat_connection_status,
            sidebar_available: state.sidebar_available, sidebar_open: state.sidebar_open, chat_actions: state.chat_actions)
        apply(to: content, dock: dock, modal: state.modal, menu: menu, newChat: newChat, send: send)
    }

    static func apply(to content: ContentController, dock: NativeBackChatDock, modal: Bool, menu: UIMenu, newChat: UIBarButtonItem? = nil,
                      send: @escaping (NativeIntent) -> Void) {
        content.connectedStatusView = nil
        content.navigationItem.title = nil
        content.navigationItem.titleView = nil
        content.navigationItem.leftBarButtonItems = []
        content.navigationItem.rightBarButtonItems = []
        content.navigationItem.trailingItemGroups = []
        // UIKit always places the system Back first, so the dock owns Back to lead with health.
        content.navigationItem.hidesBackButton = true
        content.navigationItem.leftItemsSupplementBackButton = false
        content.navigationItem.backAction = nil
        var groups = [NativeActivityRailChrome.status(on: content, status: dock.connection_status)]
        if !modal {
            groups.append(NativeActivityRailChrome.back(label: dock.back_label, enabled: true, send: send))
            if let newChat {
                groups.append(NativeActivityRailChrome.group(newChat))
            }
            if dock.sidebar_available {
                let title = dock.sidebar_open ? "Hide Agents" : "Show Agents"
                let sidebar = UIBarButtonItem(title: title, image: UIImage(systemName: "sidebar.left"),
                    primaryAction: UIAction(title: title) { _ in send(.toggle_sidebar) })
                sidebar.accessibilityLabel = title
                sidebar.accessibilityIdentifier = "native-toggle-agents"
                groups.append(UIBarButtonItemGroup(barButtonItems: [sidebar], representativeItem: nil))
            }
        }
        // Leading navigation groups enter Duo's shared bar below status and above tabs.
        // A bottom toolbar would put these controls beside the tab group instead.
        content.navigationItem.leadingItemGroups = groups
        guard !modal else {
            content.navigationItem.additionalOverflowItems = nil
            return
        }
        content.navigationItem.additionalOverflowItems = UIDeferredMenuElement.uncached { completion in
            completion(menu.children)
        }
    }
}
