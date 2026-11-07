import UIKit

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
        font = UIFontMetrics(forTextStyle: .caption2).scaledFont(
            for: .monospacedDigitSystemFont(ofSize: 11, weight: .regular))
        adjustsFontForContentSizeCategory = true
        textColor = .secondaryLabel
        textAlignment = .center
        numberOfLines = 1
        lineBreakMode = .byTruncatingTail
        adjustsFontSizeToFitWidth = true
        minimumScaleFactor = 0.75
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
                label = "\(milliseconds)ms"
                accessibilityLabel = "Connected, round trip \(milliseconds) milliseconds"
            } else {
                label = "—ms"
                accessibilityLabel = "Connected, checking round-trip time"
            }
        }
        let display = NSMutableAttributedString(string: "●", attributes: [.foregroundColor: dotColor])
        if !label.isEmpty {
            display.append(NSAttributedString(string: " " + label, attributes: [.foregroundColor: UIColor.secondaryLabel]))
        }
        attributedText = display
        isHidden = false
    }
}

@available(iOS 27.1, *)
@MainActor
enum NativeChatDock {
    private static func reservedActionSlot() -> UIBarButtonItemGroup {
        let space = UIView(frame: CGRect(x: 0, y: 0, width: 40, height: 40))
        space.isUserInteractionEnabled = false
        space.accessibilityElementsHidden = true
        space.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            space.widthAnchor.constraint(equalToConstant: 40),
            space.heightAnchor.constraint(equalToConstant: 40)
        ])
        let item = UIBarButtonItem(customView: space)
        item.isEnabled = false
        item.axisBehavior = .verticalPreferred
        item.hidesSharedBackground = true
        return UIBarButtonItemGroup(barButtonItems: [item], representativeItem: nil)
    }

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
        content.navigationItem.hidesBackButton = modal
        content.navigationItem.leftItemsSupplementBackButton = !modal
        // Duo's shared bar is outside navigationBar's interaction/accessibility exclusion.
        content.navigationItem.backAction = modal ? nil : UIAction(title: dock.back_label,
            image: UIImage(systemName: "chevron.backward")) { _ in send(.back) }
        var groups: [UIBarButtonItemGroup] = []
        // Removing a preceding action would move health into its row in Duo's shared bar.
        if modal { groups.append(reservedActionSlot()) }
        if dock.sidebar_available {
            if modal {
                groups.append(reservedActionSlot())
            } else {
                let title = dock.sidebar_open ? "Hide Agents" : "Show Agents"
                let sidebar = UIBarButtonItem(title: title, image: UIImage(systemName: "sidebar.left"),
                    primaryAction: UIAction(title: title) { _ in send(.toggle_sidebar) })
                sidebar.accessibilityLabel = title
                sidebar.accessibilityIdentifier = "native-toggle-agents"
                groups.append(UIBarButtonItemGroup(barButtonItems: [sidebar], representativeItem: nil))
            }
        }
        if let status = dock.connection_status {
            let health = NativeConnectionDockView(frame: .zero)
            health.render(status)
            let item = UIBarButtonItem(customView: health)
            item.axisBehavior = .verticalPreferred
            groups.append(UIBarButtonItemGroup(barButtonItems: [item], representativeItem: nil))
            content.connectedStatusView = health
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
