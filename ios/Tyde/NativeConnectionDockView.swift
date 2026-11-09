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
enum NativeChatDock {
    static func owner(for traits: UITraitCollection) -> NativeChatDockOwner {
        traits.verticalBarEdge == .unspecified ? .client : .shell
    }

    static func apply(to content: ContentController, state: NativePresentation, menu: UIMenu,
                      send: @escaping (NativeIntent) -> Void) throws {
        guard state.chat_header_owner == .client, let backLabel = state.chat_back_label else {
            throw HostError("Native chat dock requires its chat-local heading and Back label")
        }
        let dock = NativeBackChatDock(back_label: backLabel, connection_status: state.chat_connection_status,
            sidebar_available: state.sidebar_available, sidebar_open: state.sidebar_open, chat_actions: state.chat_actions)
        apply(to: content, dock: dock, modal: state.modal, menu: menu, send: send)
    }

    static func apply(to content: ContentController, dock: NativeBackChatDock, modal: Bool, menu: UIMenu,
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
        var groups: [UIBarButtonItemGroup] = []
        let health = NativeConnectionDockView(frame: .zero)
        if let status = dock.connection_status {
            health.render(status)
            content.connectedStatusView = health
        } else {
            // Without health, Back would rise into its row in Duo's shared bar.
            health.reserveSlot()
        }
        // Leave the item enabled: disabling it greys the label, including its status dot.
        let healthItem = UIBarButtonItem(customView: health)
        healthItem.axisBehavior = .verticalPreferred
        healthItem.hidesSharedBackground = true
        groups.append(UIBarButtonItemGroup(barButtonItems: [healthItem], representativeItem: nil))
        if !modal {
            let back = UIBarButtonItem(title: dock.back_label, image: UIImage(systemName: "chevron.backward"),
                primaryAction: UIAction(title: dock.back_label) { _ in send(.back) })
            back.accessibilityLabel = dock.back_label
            // Back keeps its own glass; adjacent leading items would otherwise merge into one capsule.
            back.sharesBackground = false
            groups.append(UIBarButtonItemGroup(barButtonItems: [back], representativeItem: nil))
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
