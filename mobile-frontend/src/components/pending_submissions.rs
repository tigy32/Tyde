//! Recovery surface for outbound submissions the transport could not account
//! for.
//!
//! Records are held silently while they are on their way out. This surface
//! exists **only** for the ones that failed, so the happy path never leaves an
//! artifact the user has to dismiss.
//!
//! New-chat records are **host-scoped**, not chat-scoped: a new-chat submission
//! has no agent, because the agent does not exist yet, and picking whichever
//! `NewAgent` showed up and calling it "ours" is a guess the client is not
//! entitled to make. The host surface therefore stays reachable wherever the
//! user navigates, without ever claiming agent ownership. Records addressed to
//! an agent that already exists *do* have known ownership — that is the agent we
//! sent to — so those render inside that chat.
//!
//! Nothing here is automatic. The client never resends on its own, never writes
//! a composer the user is already using, and never claims a message was
//! delivered.

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::bridge::LocalSubmissionId;
use crate::state::{
    AgentRef, AppState, MobileShellError, PendingSubmission, PendingSubmissionState,
    SubmissionTarget, SubmissionWithdrawal,
};

/// Minimum touch-target size, applied inline rather than through the stylesheet.
/// It is an accessibility invariant of *this* surface: the controls that recover
/// a user's lost text must stay tappable even if a stylesheet is missing,
/// restyled, or still loading.
const TOUCH_TARGET: &str = "min-width:44px;min-height:44px;";

/// The held text: selectable, and wrapped.
///
/// `overflow-wrap:anywhere` is load-bearing and therefore inline, like
/// [`TOUCH_TARGET`]. `pre-wrap` alone breaks at spaces, so a long unbroken token
/// — a URL, a base64 blob, a stack trace — would widen the page and push the
/// recovery controls off the right-hand edge of the phone. The buttons that give
/// the user their message back cannot be the ones that scroll away.
const HELD_TEXT: &str = "user-select:text;-webkit-user-select:text;white-space:pre-wrap;\
     overflow-wrap:anywhere;word-break:break-word;";

fn shell_error(state: &AppState, message: String) {
    state.mobile_shell_error.set(Some(MobileShellError {
        code: protocol::MobileAccessErrorCode::TransportFailed,
        message,
    }));
}

/// Heading for the host-scoped surface.
///
/// **It must never claim a failure the client cannot prove.** `DeliveryUnknown`
/// means Tyde *cannot tell* whether the host received the message — saying it
/// "could not be sent" is a false failure claim, and it sits directly above
/// **Send again**, which for a new chat starts a second agent, a second backend
/// session, and a second paid turn.
///
/// The model is careful never to claim a false *success*. The claim that costs
/// the user money is the false *failure*, so it is guarded just as hard: the
/// heading distinguishes "definitely did not go" from "cannot tell", and says so
/// before the user reaches for the expensive button.
fn host_heading(records: &[PendingSubmission]) -> String {
    let count = records.len();
    let unknown = records
        .iter()
        .filter(|record| record.state == PendingSubmissionState::DeliveryUnknown)
        .count();
    let definitely_not_sent = count - unknown;

    match (count, unknown, definitely_not_sent) {
        (1, 0, _) => "1 new chat was not sent".to_owned(),
        (1, _, 0) => "1 new chat may or may not have started".to_owned(),
        (n, 0, _) => format!("{n} new chats were not sent"),
        (n, _, 0) => format!("{n} new chats may or may not have started"),
        // Mixed: any wording that generalises would be false about half of them.
        // Stay neutral and let each card state its own outcome.
        (n, _, _) => format!("{n} new chats need your attention"),
    }
}

/// A resend is only offered on a connection that is not the one that swallowed
/// the original.
fn can_resend(state: &AppState, record: &PendingSubmission) -> bool {
    let current = state
        .active_connection_instance_ids
        .get()
        .get(&record.local_host_id)
        .copied();
    record.can_resend_on(current)
}

async fn copy_to_clipboard(text: &str) -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    let promise = window.navigator().clipboard().write_text(text);
    wasm_bindgen_futures::JsFuture::from(promise).await.is_ok()
}

/// Send `record`'s text again as a brand-new submission.
///
/// The new submission gets its own record, and the old one is retired **only
/// once the new one is admitted** — so the text always has a holder, even if the
/// resend is itself rejected.
async fn resend(state: &AppState, record: &PendingSubmission) {
    let outcome = match &record.target {
        SubmissionTarget::NewChat => {
            crate::actions::spawn_new_chat(state, record.text.clone(), record.images.clone()).await
        }
        SubmissionTarget::Agent(agent_ref) => {
            let stream = state.agents.with_untracked(|agents| {
                agents
                    .iter()
                    .find(|a| {
                        a.local_host_id == agent_ref.local_host_id
                            && a.agent_id == agent_ref.agent_id
                    })
                    .map(|a| a.instance_stream.clone())
            });
            let Some(stream) = stream else {
                shell_error(
                    state,
                    "Could not send again: that conversation is no longer available.".to_owned(),
                );
                return;
            };
            let payload = protocol::SendMessagePayload {
                message: record.text.clone(),
                // The held attachments, shaped exactly as a first send shapes
                // them. This was hardcoded `None`, which silently dropped every
                // image the record was holding — and `None` vs `Some(vec)` are
                // different bytes on the wire, so it was not even the same message.
                images: record.wire_images(),
                origin: None,
                // Carried through verbatim. A plan decision's payload *is* the
                // tool response, with empty message text — dropping it here would
                // resend an empty chat message and leave the agent still waiting.
                tool_response: record.tool_response.clone(),
            };
            crate::send::send_frame(
                &agent_ref.local_host_id,
                stream,
                protocol::FrameKind::SendMessage,
                &payload,
            )
            .await
        }
    };

    match outcome {
        Ok(accepted) => {
            state.hold_submission(PendingSubmission {
                local_submission_id: accepted.local_submission_id,
                // **Inherited, not minted.** This is the same logical message the
                // user sent; only the transport attempt is new. The tool card that
                // created it tracks it by this, and would otherwise lose sight of
                // it the moment the user pressed Send again.
                origin: record.origin,
                local_host_id: record.local_host_id.clone(),
                connection_instance_id: accepted.connection_instance_id,
                target: record.target.clone(),
                text: record.text.clone(),
                images: record.images.clone(),
                // Carried onto the new record too, or a resend that fails *again*
                // would come back as a plain empty message with the decision gone.
                tool_response: record.tool_response.clone(),
                state: PendingSubmissionState::QueuedLocally,
            });
            // Retire the superseded *attempt* — not the lineage. The message is
            // still very much in play, so this must not tell the originating card
            // that the user withdrew it.
            state.retire_submission_attempt(record.local_submission_id);
        }
        Err(error) => shell_error(state, format!("Could not send again: {error}")),
    }
}

/// One failed submission, with the controls to recover it.
#[component]
fn PendingSubmissionCard(local_submission_id: LocalSubmissionId) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

    // Look the record up reactively: a keyed row is created once per key, so
    // reading fields off a snapshot taken at creation would freeze the row.
    //
    // Both this and `with_record` below are `Copy`: precise capture means they hold
    // only the `RwSignal` (itself `Copy`) and the submission id, not the whole
    // `AppState`. Every `move` closure that calls one therefore gets its own copy
    // for free — they need no per-consumer alias, and cloning one is a no-op.
    let record = {
        let state = state.clone();
        move || {
            state
                .pending_submissions
                .with(|records| records.get(&local_submission_id).cloned())
        }
    };

    let confirming_discard = RwSignal::new(false);
    let copied = RwSignal::new(false);
    // Set while this card's resend is unsettled.
    //
    // "Send again" on a new-chat record calls `spawn_new_chat`: a second tap is a
    // second agent, a second backend session, and a second paid turn. The composer
    // grew an explicit in-flight latch for exactly this; the recovery card — which
    // is the surface a *frustrated* user is jabbing at — had none.
    //
    // It does not depend on the send resolving in the same microtask. It holds even
    // when the send genuinely yields.
    let resending = RwSignal::new(false);

    let with_record = {
        let state = state.clone();
        move || {
            state
                .pending_submissions
                .with_untracked(|r| r.get(&local_submission_id).cloned())
        }
    };

    let on_copy = move |_| {
        if resending.get_untracked() {
            return;
        }
        let Some(record) = with_record() else { return };
        spawn_local(async move {
            // Copy what the user can actually see. For a plan decision that is the
            // decision, not an empty string.
            if copy_to_clipboard(&record.display_text()).await {
                copied.set(true);
            }
        });
    };

    let edit_state = state.clone();
    let on_edit = move |_| {
        if resending.get_untracked() {
            return;
        }
        let Some(record) = with_record() else { return };
        // A plan decision is a typed answer, not chat text. Its message body is
        // empty, so "editing" it would drop nothing into the composer and quietly
        // lose the decision. Send again re-sends it correctly; Discard drops it.
        if !record.is_editable_in_composer() {
            shell_error(
                &edit_state,
                "This is a plan decision, not a message. Use Send again, or Discard it.".to_owned(),
            );
            return;
        }
        // Explicit user action is the only thing that may write the composer,
        // and anything already typed there wins: appending would silently
        // reorder the user's own words. The record stays put and says so.
        if !edit_state
            .chat_input
            .with_untracked(|text| text.trim().is_empty())
        {
            shell_error(
                &edit_state,
                "Clear the message box first, then tap Edit again.".to_owned(),
            );
            return;
        }
        edit_state.chat_input.set(record.text.clone());
        // The composer holds text; it cannot hold attachments. Retiring a record
        // that carries images would destroy the only copy of them, so a record
        // with attachments keeps its row — the text is now editable in the
        // composer, and the images stay recoverable until the user explicitly
        // discards them. Discard remains the single path that destroys anything.
        if record.images.is_empty() {
            // Terminal for the *lineage*: the message is back with the user. The
            // card that created it must say so, not revert to "queued locally".
            edit_state.withdraw_submission(
                local_submission_id,
                SubmissionWithdrawal::ReturnedToComposer,
            );
        }
    };

    let discard_state = state.clone();
    let on_discard = move |_| {
        if resending.get_untracked() {
            return;
        }
        // Two explicit taps. Discard is the only control that destroys the text,
        // so it never happens on a single press.
        if !confirming_discard.get_untracked() {
            confirming_discard.set(true);
            return;
        }
        // Terminal for the *lineage*. After this, a card that created the message
        // must never go back to claiming it is "queued locally" — the user threw
        // it away.
        discard_state.withdraw_submission(local_submission_id, SubmissionWithdrawal::Discarded);
    };

    let resend_state = state.clone();
    let on_resend = move |_| {
        if resending.get_untracked() {
            return;
        }
        let Some(record) = with_record() else {
            return;
        };
        if !can_resend(&resend_state, &record) {
            return;
        }
        resending.set(true);
        let state = resend_state.clone();
        spawn_local(async move {
            resend(&state, &record).await;
            resending.set(false);
        });
    };

    let enabled_state = state.clone();
    let resend_enabled = Memo::new(move |_| {
        !resending.get()
            && enabled_state
                .pending_submissions
                .with(|records| records.get(&local_submission_id).cloned())
                .map(|record| can_resend(&enabled_state, &record))
                .unwrap_or(false)
    });
    // Every recovery action is closed while a resend is unsettled — not just the
    // resend. Discard racing an in-flight resend would destroy the text mid-send;
    // Edit racing it would put the text in the composer *and* leave it on the wire.
    let actions_disabled = move || resending.get();

    // `DeliveryUnknown` is the only signal that an action may or may not have
    // taken effect, so it is an assertive alert rather than a status, and it
    // persists until the user resolves it — never a self-dismissing toast.
    let role = move || match record().map(|r| r.state) {
        Some(PendingSubmissionState::DeliveryUnknown) => "alert",
        _ => "status",
    };
    let live = move || match record().map(|r| r.state) {
        Some(PendingSubmissionState::DeliveryUnknown) => "assertive",
        _ => "polite",
    };

    let label = move || record().map(|r| r.state_label()).unwrap_or_default();
    let detail = move || record().map(|r| r.state_detail()).unwrap_or_default();
    // `display_text`, not `text`: a plan decision's message body is empty — the
    // payload *is* the decision — so rendering `text` would show the user a blank
    // box and tell them nothing about what they are recovering.
    let text = move || record().map(|r| r.display_text()).unwrap_or_default();

    // The cost of a duplicate is real, so it is stated next to the control that
    // can cause it — and only when the message might actually have landed.
    let duplicate_warning = move || {
        let record = record()?;
        match (record.state, &record.target) {
            (PendingSubmissionState::DeliveryUnknown, SubmissionTarget::NewChat) => Some(
                "Sending again may start a second agent, and bill for it, if the first one did \
                 reach your computer.",
            ),
            (PendingSubmissionState::DeliveryUnknown, SubmissionTarget::Agent(_)) => Some(
                "Sending again may deliver this message twice if the first one did reach your \
                 computer.",
            ),
            _ => None,
        }
    };

    view! {
        <li
            class="pending-submission"
            role=role
            aria-live=live
            data-mobile-test="pending-submission"
        >
            <div class="pending-submission-state" data-mobile-test="pending-submission-state">
                {label}
            </div>
            <div class="pending-submission-detail">{detail}</div>
            // Selectable and copyable: the escape hatch that still works even if
            // every other control fails the user.
            <p
                class="pending-submission-text"
                style=HELD_TEXT
                data-mobile-test="pending-submission-text"
            >
                {text}
            </p>
            <div class="pending-submission-actions">
                <button
                    type="button"
                    class="pending-submission-action"
                    style=TOUCH_TARGET
                    data-mobile-test="pending-submission-copy"
                    aria-label="Copy message text"
                    disabled=actions_disabled
                    on:click=on_copy
                >
                    {move || if copied.get() { "Copied" } else { "Copy" }}
                </button>
                <button
                    type="button"
                    class="pending-submission-action"
                    style=TOUCH_TARGET
                    data-mobile-test="pending-submission-edit"
                    aria-label="Move message back to the message box"
                    disabled=actions_disabled
                    on:click=on_edit
                >
                    "Edit"
                </button>
                <button
                    type="button"
                    class="pending-submission-action pending-submission-discard"
                    style=TOUCH_TARGET
                    data-mobile-test="pending-submission-discard"
                    aria-label=move || {
                        if confirming_discard.get() {
                            "Confirm discarding this message permanently"
                        } else {
                            "Discard this message"
                        }
                    }
                    disabled=actions_disabled
                    on:click=on_discard
                >
                    {move || {
                        if confirming_discard.get() { "Tap again to discard" } else { "Discard" }
                    }}
                </button>
                // Resend is last, distinctly labelled, never styled as the
                // default action, disabled until a genuinely new connection
                // exists, and never automatic.
                <button
                    type="button"
                    class="pending-submission-action pending-submission-resend"
                    style=TOUCH_TARGET
                    data-mobile-test="pending-submission-resend"
                    aria-label="Send this message again"
                    disabled=move || !resend_enabled.get()
                    on:click=on_resend
                >
                    {move || if resending.get() { "Sending…" } else { "Send again" }}
                </button>
            </div>
            {move || {
                duplicate_warning()
                    .map(|warning| {
                        view! {
                            <p
                                class="pending-submission-warning"
                                data-mobile-test="pending-submission-resend-warning"
                            >
                                {warning}
                            </p>
                        }
                    })
            }}
            {move || {
                (!resend_enabled.get())
                    .then(|| {
                        view! {
                            <p
                                class="pending-submission-hint"
                                data-mobile-test="pending-submission-reconnect-hint"
                            >
                                "Reconnect to this host to send again."
                            </p>
                        }
                    })
            }}
        </li>
    }
}

/// Host-scoped surface for new-chat submissions that failed in transit.
///
/// Renders nothing when there is nothing to recover.
#[component]
pub fn PendingSubmissions() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

    let list_state = state.clone();
    let records = Memo::new(move |_| {
        let Some(host) = list_state.active_local_host_id.get() else {
            return Vec::new();
        };
        list_state.surfaced_new_chat_submissions(&host)
    });

    view! {
        {move || {
            let rows = records.get();
            if rows.is_empty() {
                return view! { <div></div> }.into_any();
            }
            view! {
                <section
                    class="pending-submissions"
                    aria-label="Messages that need your attention"
                    data-mobile-test="pending-submissions"
                >
                    <h2
                        class="pending-submissions-title"
                        data-mobile-test="pending-submissions-title"
                    >
                        {host_heading(&rows)}
                    </h2>
                    <ul class="pending-submissions-list">
                        <For
                            each=move || records.get()
                            key=|record| record.local_submission_id
                            let:record
                        >
                            <PendingSubmissionCard local_submission_id=record.local_submission_id />
                        </For>
                    </ul>
                </section>
            }
                .into_any()
        }}
    }
}

/// Chat-scoped recovery rows for submissions addressed to `agent_ref`.
/// Ownership is known here — that is the agent we sent to.
#[component]
pub fn AgentPendingSubmissions(agent_ref: AgentRef) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

    let list_state = state.clone();
    let records = Memo::new(move |_| list_state.surfaced_agent_submissions(&agent_ref));

    view! {
        {move || {
            let rows = records.get();
            if rows.is_empty() {
                return view! { <div></div> }.into_any();
            }
            view! {
                // These rows materialise without the user doing anything, so a
                // screen-reader user meets them mid-transcript. The list needs a
                // name saying what it is; each card carries its own
                // status/alert live region announcing why it appeared.
                <ul
                    class="pending-submissions-list"
                    aria-label="Messages in this conversation that need your attention"
                    data-mobile-test="agent-pending-submissions"
                >
                    <For
                        each=move || records.get()
                        key=|record| record.local_submission_id
                        let:record
                    >
                        <PendingSubmissionCard local_submission_id=record.local_submission_id />
                    </For>
                </ul>
            }
                .into_any()
        }}
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{LocalHostId, SubmissionOriginId};
    use leptos::mount::mount_to;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    const HOST: &str = "host-1";

    /// Base id for records a fixture *fabricates*.
    ///
    /// The send seam mints `LocalSubmissionId` from its own attempt counter, which
    /// starts at **1** (`bridge/web/connection.rs`). So a fixture that seeds a record
    /// at a low id collides with the id the next send in the same test will be handed:
    /// `resend()` holds the replacement at that key — **overwriting the record it is
    /// replacing** — and then retires the superseded attempt by the same key, deleting
    /// it. The message disappears.
    ///
    /// Production cannot reach this. `next_local_submission_id` is a single monotonic
    /// counter on the connection manager for the whole process, so an id it mints is
    /// never one already held. Only a fixture that fabricates ids can collide with it,
    /// so fixtures stay clear of its range.
    const FIXTURE_ID: u64 = 9_000;

    /// Base for origins a fixture fabricates.
    ///
    /// Same hazard as [`FIXTURE_ID`], different counter: `mint_submission_origin`
    /// also counts up from 0, so a fabricated origin can collide with one the product
    /// is about to mint — and two records sharing a lineage id would make
    /// `submission_lifecycle` report one message's fate for another.
    const FIXTURE_ORIGIN: u64 = 9_000;

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    /// Mount the host surface with one record in `record_state`, admitted on
    /// connection instance 7. `live_instance` is the connection the client is on
    /// *now* — `Some(8)` means a genuinely new connection exists.
    async fn mount_with(
        record_state: PendingSubmissionState,
        live_instance: Option<u64>,
    ) -> (HtmlElement, AppState) {
        let container = make_container();
        let handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let handle_for_mount = handle.clone();
        let h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let host = LocalHostId(HOST.to_owned());
            state.active_local_host_id.set(Some(host.clone()));
            if let Some(instance) = live_instance {
                state.active_connection_instance_ids.update(|m| {
                    m.insert(host.clone(), instance);
                });
            }
            // `hold_submission`, not a raw `insert`: it keys the map by the record's
            // own id, so the fixture cannot seed a key that disagrees with the record
            // it holds — which is exactly what the hand-rolled insert had done.
            state.hold_submission(PendingSubmission {
                local_submission_id: LocalSubmissionId(FIXTURE_ID),
                origin: SubmissionOriginId(FIXTURE_ORIGIN),
                local_host_id: host,
                connection_instance_id: 7,
                target: SubmissionTarget::NewChat,
                text: "deploy the thing".to_owned(),
                images: Vec::new(),
                tool_response: None,
                state: record_state,
            });
            *handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <PendingSubmissions /> }
        });
        // The mount handle must outlive the assertions, and this helper *returns*.
        //
        // Dropping it unmounts the view and disposes the reactive owner — which owns
        // every signal `AppState::new()` created inside the closure above. So the
        // caller was handed a torn-down container and an `AppState` full of disposed
        // signals: `find(...)` found nothing, and reading `pending_submissions`
        // panicked with "already been disposed".
        //
        // Worse than the failures: tests asserting *absence* passed vacuously against
        // an empty DOM. Every other harness in the crate — `chat_view`, `tool_card`,
        // `chat_input`, `app`, and `main.rs` itself — forgets the handle. This one is
        // the outlier.
        std::mem::forget(h);
        next_tick().await;
        let state = handle.borrow().as_ref().unwrap().clone();
        (container, state)
    }

    fn find(container: &HtmlElement, test_id: &str) -> Option<web_sys::Element> {
        container
            .query_selector(&format!("[data-mobile-test='{test_id}']"))
            .unwrap()
    }

    /// The happy path leaves no artifact. A submission still on its way out is
    /// held silently — the user is not shown a pending banner they would have to
    /// dismiss on every single message.
    #[wasm_bindgen_test]
    async fn an_in_flight_submission_shows_nothing_at_all() {
        let (container, _state) = mount_with(PendingSubmissionState::QueuedLocally, Some(7)).await;
        assert!(
            find(&container, "pending-submissions").is_none(),
            "a queued submission must not surface anything: {}",
            container.text_content().unwrap_or_default()
        );
    }

    /// `DeliveryUnknown` is the one state that says an action may or may not have
    /// taken effect. It is an assertive alert, it names what it does not know,
    /// and it offers every recovery route.
    #[wasm_bindgen_test]
    async fn delivery_unknown_surfaces_as_a_persistent_alert_with_full_recovery() {
        let (container, state) = mount_with(PendingSubmissionState::DeliveryUnknown, Some(8)).await;

        let row = find(&container, "pending-submission").expect("a failed submission must surface");
        assert_eq!(
            row.get_attribute("role").as_deref(),
            Some("alert"),
            "an action that may or may not have applied must interrupt, not sit in a polite status"
        );

        let text = find(&container, "pending-submission-text")
            .expect("the user's text must be shown so they can recover it")
            .text_content()
            .unwrap_or_default();
        assert!(
            text.contains("deploy the thing"),
            "the exact text must be recoverable, got: {text}"
        );

        for control in [
            "pending-submission-copy",
            "pending-submission-edit",
            "pending-submission-discard",
            "pending-submission-resend",
        ] {
            assert!(
                find(&container, control).is_some(),
                "recovery control '{control}' must be available"
            );
        }

        let body = container.text_content().unwrap_or_default().to_lowercase();
        assert!(
            !body.contains("delivered") && !body.contains("message sent"),
            "the client cannot know the message arrived and must never claim it did: {body}"
        );
        // The symmetrical guard, and the one that costs money. Claiming a
        // failure Tyde cannot prove is what pushes a user toward "Send again",
        // which for a new chat starts — and bills for — a second agent.
        assert!(
            !body.contains("could not be sent")
                && !body.contains("was not sent")
                && !body.contains("were not sent")
                && !body.contains("failed to send"),
            "a delivery-unknown message must never be described as a failure — Tyde \
             cannot tell whether it arrived: {body}"
        );

        // It persists: nothing auto-dismisses it, and no timer clears it.
        next_tick().await;
        next_tick().await;
        assert!(
            find(&container, "pending-submission").is_some(),
            "a delivery-unknown alert must persist until the user resolves it"
        );
        assert_eq!(
            state.pending_submissions.get_untracked().len(),
            1,
            "nothing may retire the record behind the user's back"
        );
    }

    /// The heading is the first thing read and it frames the decision to tap the
    /// billable "Send again". It must distinguish "definitely did not go" from
    /// "cannot tell", and it must never claim the second is the first.
    #[wasm_bindgen_test]
    async fn the_host_heading_never_calls_an_unknown_delivery_a_failure() {
        let (container, _state) =
            mount_with(PendingSubmissionState::DeliveryUnknown, Some(8)).await;
        let heading = find(&container, "pending-submissions-title")
            .expect("the host surface must have a heading")
            .text_content()
            .unwrap_or_default()
            .to_lowercase();
        assert!(
            !heading.contains("not sent") && !heading.contains("could not be sent"),
            "an unknown delivery is not a failure, and the heading sits right above \
             the button that starts a second paid agent: {heading}"
        );
        assert!(
            heading.contains("may or may not"),
            "the heading must say what is actually known — that Tyde cannot tell: {heading}"
        );
    }

    /// A `NotSent` message *is* provably a failure, and saying so is what tells
    /// the user it is free to send again. The heading must not blur the two.
    #[wasm_bindgen_test]
    async fn the_host_heading_states_a_provable_failure_plainly() {
        let (container, _state) = mount_with(PendingSubmissionState::NotSent, Some(8)).await;
        let heading = find(&container, "pending-submissions-title")
            .expect("the host surface must have a heading")
            .text_content()
            .unwrap_or_default()
            .to_lowercase();
        assert!(
            heading.contains("was not sent"),
            "a provably-unsent message must be named as such, so the user knows a \
             resend is free: {heading}"
        );
        assert!(
            !heading.contains("may or may not"),
            "hedging a fact Tyde *does* know is its own false claim: {heading}"
        );
    }

    /// A mixed surface cannot generalise: any wording that fits one record lies
    /// about the other. It stays neutral and lets each card speak.
    #[wasm_bindgen_test]
    async fn a_mixed_host_surface_makes_no_blanket_claim() {
        let container = make_container();
        let handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let handle_for_mount = handle.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let host = LocalHostId(HOST.to_owned());
            state.active_local_host_id.set(Some(host.clone()));
            for (id, record_state) in [
                (1u64, PendingSubmissionState::NotSent),
                (2u64, PendingSubmissionState::DeliveryUnknown),
            ] {
                state.hold_submission(PendingSubmission {
                    local_submission_id: LocalSubmissionId(FIXTURE_ID + id),
                    origin: SubmissionOriginId(FIXTURE_ORIGIN + id),
                    local_host_id: host.clone(),
                    connection_instance_id: 7,
                    target: SubmissionTarget::NewChat,
                    text: format!("message {id}"),
                    images: Vec::new(),
                    tool_response: None,
                    state: record_state,
                });
            }
            *handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <PendingSubmissions /> }
        });
        next_tick().await;

        let heading = find(&container, "pending-submissions-title")
            .unwrap()
            .text_content()
            .unwrap_or_default()
            .to_lowercase();
        assert!(
            !heading.contains("not sent") && !heading.contains("may or may not"),
            "a blanket claim over a mixed set is false about half of it: {heading}"
        );
        assert!(
            heading.contains("need your attention"),
            "a mixed surface must stay neutral and let each card state its own \
             outcome: {heading}"
        );
    }

    /// Resending a new chat can start — and bill for — a second agent. That cost
    /// is stated next to the control that causes it.
    #[wasm_bindgen_test]
    async fn resending_a_new_chat_warns_it_may_create_a_second_agent() {
        let (container, _state) =
            mount_with(PendingSubmissionState::DeliveryUnknown, Some(8)).await;
        let warning = find(&container, "pending-submission-resend-warning")
            .expect("an ambiguous new-chat resend must warn about a second agent")
            .text_content()
            .unwrap_or_default();
        assert!(
            warning.contains("second agent") && warning.contains("bill"),
            "the warning must name both the duplicate agent and its cost, got: {warning}"
        );
    }

    /// `NotSent` is provably never transmitted, so it is safe to send again and
    /// says so — it must not carry the duplicate warning that `DeliveryUnknown`
    /// does, or the two states become indistinguishable and the warning becomes
    /// noise.
    #[wasm_bindgen_test]
    async fn not_sent_is_definite_and_carries_no_duplicate_warning() {
        let (container, _state) = mount_with(PendingSubmissionState::NotSent, Some(8)).await;

        let detail = container.text_content().unwrap_or_default();
        assert!(
            detail.contains("definitely never") && detail.contains("safe"),
            "a provably-unsent message must say it is safe to resend, got: {detail}"
        );
        assert!(
            find(&container, "pending-submission-resend-warning").is_none(),
            "a message that provably never left must not warn about duplicates"
        );
    }

    /// Resend is never automatic, and never offered on the connection that
    /// already swallowed the message.
    #[wasm_bindgen_test]
    async fn resend_is_disabled_until_a_new_connection_exists() {
        let (container, _state) =
            mount_with(PendingSubmissionState::DeliveryUnknown, Some(7)).await;
        let resend = find(&container, "pending-submission-resend").expect("resend must render");
        assert!(
            resend.has_attribute("disabled"),
            "resending on the same connection that lost the message is not recovery"
        );
        assert!(
            find(&container, "pending-submission-reconnect-hint").is_some(),
            "the user must be told what would make a resend possible"
        );

        let (container, _state) =
            mount_with(PendingSubmissionState::DeliveryUnknown, Some(8)).await;
        let resend = find(&container, "pending-submission-resend").expect("resend must render");
        assert!(
            !resend.has_attribute("disabled"),
            "a genuinely new connection is what enables a deliberate resend"
        );
    }

    /// **A double-tap on "Send again" must not buy a second agent.**
    ///
    /// This is the composer's double-submit hazard, on the surface a *frustrated*
    /// user is jabbing at. `Send again` on a new-chat record calls `spawn_new_chat`:
    /// two taps is two agents, two backend sessions, and two paid turns. The
    /// composer grew an explicit in-flight latch for this; the recovery card had
    /// none, and only escaped by the accident of the send resolving in the same
    /// microtask.
    ///
    /// So the send is made to genuinely yield, and then tapped three times.
    #[wasm_bindgen_test]
    async fn a_double_tap_on_send_again_emits_exactly_one_frame() {
        let _guard = crate::bridge::test_defer_sends();
        // A live, *different* connection instance, so resend is enabled.
        let (container, state) = mount_with(PendingSubmissionState::DeliveryUnknown, Some(8)).await;
        // `spawn_new_chat` needs a host stream and a backend to build the frame.
        state.host_streams.update(|m| {
            m.insert(
                LocalHostId(HOST.to_owned()),
                protocol::StreamPath("/host/h1".to_owned()),
            );
        });
        state
            .draft_backend_override
            .set(Some(protocol::BackendKind::Claude));
        next_tick().await;

        let resend: HtmlElement = find(&container, "pending-submission-resend")
            .unwrap()
            .dyn_into()
            .unwrap();
        assert!(
            !resend.has_attribute("disabled"),
            "resend must start enabled"
        );

        resend.click();
        next_tick().await;

        assert_eq!(
            crate::bridge::test_send_attempts(),
            1,
            "the first tap sends exactly one SpawnAgent"
        );
        assert!(
            resend.has_attribute("disabled"),
            "Send again must be disabled while the resend is unsettled"
        );
        // Every other recovery action is closed too: Discard racing an in-flight
        // resend would destroy the text mid-send, and Edit would put it in the
        // composer *and* leave it on the wire.
        for control in [
            "pending-submission-copy",
            "pending-submission-edit",
            "pending-submission-discard",
        ] {
            assert!(
                find(&container, control).unwrap().has_attribute("disabled"),
                "'{control}' must be closed while a resend is unsettled"
            );
        }

        resend.click();
        next_tick().await;
        resend.click();
        next_tick().await;

        assert_eq!(
            crate::bridge::test_send_attempts(),
            1,
            "an impatient double-tap on Send again must not start a second agent"
        );
    }

    /// A deliberate resend replaces the *attempt*, not the message. The logical
    /// submission survives with the same identity, so whatever created it still
    /// recognises it.
    #[wasm_bindgen_test]
    async fn a_resend_inherits_the_logical_identity_of_the_message_it_replaces() {
        let _guard = crate::bridge::test_capture_sends();
        let (container, state) = mount_with(PendingSubmissionState::DeliveryUnknown, Some(8)).await;
        state.host_streams.update(|m| {
            m.insert(
                LocalHostId(HOST.to_owned()),
                protocol::StreamPath("/host/h1".to_owned()),
            );
        });
        state
            .draft_backend_override
            .set(Some(protocol::BackendKind::Claude));
        next_tick().await;

        let original = state
            .pending_submissions
            .get_untracked()
            .values()
            .next()
            .cloned()
            .unwrap();

        let resend: HtmlElement = find(&container, "pending-submission-resend")
            .unwrap()
            .dyn_into()
            .unwrap();
        resend.click();
        next_tick().await;
        next_tick().await;

        let replacement = state
            .pending_submissions
            .get_untracked()
            .values()
            .next()
            .cloned()
            .expect("the resend must be held");

        assert_ne!(
            replacement.local_submission_id, original.local_submission_id,
            "a resend is a new transport attempt and gets a new attempt id"
        );
        assert_eq!(
            replacement.origin, original.origin,
            "…but it is the same message, so its logical identity is inherited — \
             otherwise the card that created it loses track of its own reply"
        );
        assert!(
            state.withdrawn_submissions.get_untracked().is_empty(),
            "superseding an attempt is not the user withdrawing the message, and \
             must not be recorded as one"
        );
    }

    /// Discard destroys the only copy of the user's text, so one stray tap must
    /// never do it.    /// Discard destroys the only copy of the user's text, so one stray tap must
    /// never do it.
    #[wasm_bindgen_test]
    async fn discard_takes_two_deliberate_taps() {
        let (container, state) = mount_with(PendingSubmissionState::DeliveryUnknown, Some(8)).await;

        let discard: HtmlElement = find(&container, "pending-submission-discard")
            .unwrap()
            .dyn_into()
            .unwrap();
        discard.click();
        next_tick().await;

        assert_eq!(
            state.pending_submissions.get_untracked().len(),
            1,
            "one tap must not destroy the user's text"
        );
        let label = discard.text_content().unwrap_or_default();
        assert!(
            label.to_lowercase().contains("again"),
            "the control must ask for confirmation, got: {label}"
        );

        discard.click();
        next_tick().await;
        assert!(
            state.pending_submissions.get_untracked().is_empty(),
            "a second, deliberate tap discards"
        );
    }

    /// Every recovery control has to be tappable on a phone. Asserted as real
    /// geometry, not as a class name — the size is inline precisely so it cannot
    /// be lost to a stylesheet change.
    ///
    /// Measured with `offsetWidth`/`offsetHeight`, the rendered border box, which is
    /// exactly the hit area a thumb lands in. (`getBoundingClientRect` returns a
    /// `DomRect`, a `web-sys` feature this crate does not enable — and it would buy
    /// nothing here: it differs only by CSS transforms, and these tests mount
    /// without a stylesheet, so there are none to apply.)
    ///
    /// Every button on the card is measured, not a hardcoded four: a fifth recovery
    /// control added later must clear the same bar without anyone remembering to
    /// come back and extend this list.
    #[wasm_bindgen_test]
    async fn recovery_controls_meet_the_minimum_touch_target() {
        let (container, _state) =
            mount_with(PendingSubmissionState::DeliveryUnknown, Some(8)).await;

        let card = find(&container, "pending-submission").expect("the card must render");
        let buttons = card.query_selector_all("button").unwrap();
        assert!(
            buttons.length() >= 4,
            "expected at least Copy, Edit, Discard and Send again, found {}",
            buttons.length()
        );

        for index in 0..buttons.length() {
            let control: HtmlElement = buttons.item(index).unwrap().dyn_into().unwrap();
            let label = control.text_content().unwrap_or_default().trim().to_owned();
            let (width, height) = (control.offset_width(), control.offset_height());
            assert!(
                width >= 44 && height >= 44,
                "recovery control '{label}' must be at least 44x44 to be tappable, \
                 got {width}x{height}"
            );
        }
    }

    /// The agent-scoped list materialises inside a transcript without the user
    /// doing anything. A screen-reader user meeting an unnamed list mid-chat has
    /// no way to know what it is or why it appeared.
    #[wasm_bindgen_test]
    async fn the_agent_scoped_list_has_an_accessible_name() {
        let container = make_container();
        let agent_ref = AgentRef {
            local_host_id: LocalHostId(HOST.to_owned()),
            agent_id: protocol::AgentId("agent-1".to_owned()),
        };
        let agent_for_mount = agent_ref.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let host = LocalHostId(HOST.to_owned());
            state.active_local_host_id.set(Some(host.clone()));
            state.hold_submission(PendingSubmission {
                local_submission_id: LocalSubmissionId(FIXTURE_ID),
                origin: SubmissionOriginId(FIXTURE_ORIGIN + 2),
                local_host_id: host,
                connection_instance_id: 7,
                target: SubmissionTarget::Agent(agent_for_mount.clone()),
                text: "to this agent".to_owned(),
                images: Vec::new(),
                tool_response: None,
                state: PendingSubmissionState::DeliveryUnknown,
            });
            provide_context(state);
            view! { <AgentPendingSubmissions agent_ref=agent_for_mount.clone() /> }
        });
        next_tick().await;

        let list = find(&container, "agent-pending-submissions")
            .expect("an agent-addressed failure must surface inside its own chat");
        let name = list
            .get_attribute("aria-label")
            .expect("the list must be named — it appears unbidden in the transcript");
        assert!(
            !name.trim().is_empty(),
            "an empty accessible name is the same as none"
        );
        // The card inside still carries its own live region, so its arrival is
        // announced rather than silently inserted.
        let card = find(&container, "pending-submission").expect("the card must render");
        assert_eq!(
            card.get_attribute("role").as_deref(),
            Some("alert"),
            "an unknown delivery appearing unbidden must announce itself"
        );
    }

    /// The composer holds text, not attachments. Retiring a record on Edit would
    /// destroy the only copy of its images — which is precisely the silent data
    /// loss the removed auto-restore was committing.
    #[wasm_bindgen_test]
    async fn edit_never_destroys_attachments_it_cannot_hand_back() {
        let container = make_container();
        let handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let handle_for_mount = handle.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let host = LocalHostId(HOST.to_owned());
            state.active_local_host_id.set(Some(host.clone()));
            state.hold_submission(PendingSubmission {
                local_submission_id: LocalSubmissionId(FIXTURE_ID),
                origin: SubmissionOriginId(FIXTURE_ORIGIN + 3),
                local_host_id: host,
                connection_instance_id: 7,
                target: SubmissionTarget::NewChat,
                text: "look at this".to_owned(),
                images: vec![protocol::ImageData {
                    media_type: "image/png".to_owned(),
                    data: "AAAA".to_owned(),
                }],
                tool_response: None,
                state: PendingSubmissionState::NotSent,
            });
            *handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <PendingSubmissions /> }
        });
        next_tick().await;
        let state = handle.borrow().as_ref().unwrap().clone();

        let edit: HtmlElement = find(&container, "pending-submission-edit")
            .unwrap()
            .dyn_into()
            .unwrap();
        edit.click();
        next_tick().await;

        assert_eq!(
            state.chat_input.get_untracked(),
            "look at this",
            "Edit must hand the text back"
        );
        let held = state
            .pending_submissions
            .get_untracked()
            .get(&LocalSubmissionId(FIXTURE_ID))
            .cloned()
            .expect(
                "a record carrying attachments must survive Edit — the composer cannot \
                 hold them, so discarding it would destroy the only copy",
            );
        assert_eq!(held.images.len(), 1, "the images must still be recoverable");
    }

    /// **An agent-targeted resend must put the held images on the wire.**
    ///
    /// It was hardcoding `images: None`, silently dropping every attachment the
    /// record was holding. And `SendMessagePayload::images` is
    /// `skip_serializing_if = "Option::is_none"`, so `None` and `Some(vec)` are
    /// *different bytes* — the resend was not even sending the same message.
    ///
    /// Asserted against the captured frame, not against the record: the record was
    /// always right; it was the serialization that lost them.
    #[wasm_bindgen_test]
    async fn an_agent_resend_serializes_the_held_images_onto_the_wire() {
        let _guard = crate::bridge::test_capture_sends();
        let container = make_container();
        let agent_ref = AgentRef {
            local_host_id: LocalHostId(HOST.to_owned()),
            agent_id: protocol::AgentId("agent-1".to_owned()),
        };
        let agent_for_mount = agent_ref.clone();
        let handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let handle_for_mount = handle.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let host = LocalHostId(HOST.to_owned());
            state.active_local_host_id.set(Some(host.clone()));
            // A *new* connection instance, so the resend is offered at all.
            state.active_connection_instance_ids.update(|m| {
                m.insert(host.clone(), 8);
            });
            state.agents.set(vec![crate::state::AgentInfo {
                local_host_id: host.clone(),
                agent_id: protocol::AgentId("agent-1".to_owned()),
                name: "Agent".to_owned(),
                origin: protocol::AgentOrigin::User,
                backend_kind: protocol::BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: None,
                custom_agent_id: None,
                created_at_ms: 0,
                instance_stream: protocol::StreamPath("/agent/agent-1/inst".to_owned()),
                started: true,
                fatal_error: None,
            }]);
            state.hold_submission(PendingSubmission {
                local_submission_id: LocalSubmissionId(FIXTURE_ID),
                origin: SubmissionOriginId(FIXTURE_ORIGIN + 1),
                local_host_id: host,
                connection_instance_id: 7,
                target: SubmissionTarget::Agent(agent_for_mount.clone()),
                text: "look at this".to_owned(),
                images: vec![
                    protocol::ImageData {
                        media_type: "image/png".to_owned(),
                        data: "AAAA".to_owned(),
                    },
                    protocol::ImageData {
                        media_type: "image/jpeg".to_owned(),
                        data: "BBBB".to_owned(),
                    },
                ],
                tool_response: None,
                state: PendingSubmissionState::DeliveryUnknown,
            });
            *handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <AgentPendingSubmissions agent_ref=agent_for_mount.clone() /> }
        });
        next_tick().await;
        let state = handle.borrow().as_ref().unwrap().clone();

        let resend: HtmlElement = find(&container, "pending-submission-resend")
            .unwrap()
            .dyn_into()
            .unwrap();
        resend.click();
        next_tick().await;
        next_tick().await;

        let lines = crate::bridge::test_sent_lines();
        assert_eq!(lines.len(), 1, "exactly one frame must go out");
        let frame: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        let images = frame
            .pointer("/payload/images")
            .expect("the held images must be serialized — they were being dropped entirely");
        let images = images.as_array().expect("images must be a list");
        assert_eq!(images.len(), 2, "every held image must be on the wire");
        assert_eq!(images[0]["media_type"], "image/png");
        assert_eq!(images[0]["data"], "AAAA");
        assert_eq!(images[1]["media_type"], "image/jpeg");
        assert_eq!(images[1]["data"], "BBBB");
        assert_eq!(
            frame.pointer("/payload/message").and_then(|m| m.as_str()),
            Some("look at this"),
            "and the text must still be the same message"
        );

        // The record kept its images too, so a *second* failure is still recoverable.
        let replacement = state
            .pending_submissions
            .get_untracked()
            .values()
            .next()
            .cloned()
            .unwrap();
        assert_eq!(replacement.images.len(), 2);
    }

    /// A record with no images must serialize as an *absent* field, not an empty
    /// list. `skip_serializing_if` makes those different bytes, and a resend has to
    /// produce the shape a first send produces.
    #[wasm_bindgen_test]
    async fn a_resend_with_no_images_omits_the_field_entirely() {
        let _guard = crate::bridge::test_capture_sends();
        let container = make_container();
        let agent_ref = AgentRef {
            local_host_id: LocalHostId(HOST.to_owned()),
            agent_id: protocol::AgentId("agent-1".to_owned()),
        };
        let agent_for_mount = agent_ref.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let host = LocalHostId(HOST.to_owned());
            state.active_local_host_id.set(Some(host.clone()));
            state.active_connection_instance_ids.update(|m| {
                m.insert(host.clone(), 8);
            });
            state.agents.set(vec![crate::state::AgentInfo {
                local_host_id: host.clone(),
                agent_id: protocol::AgentId("agent-1".to_owned()),
                name: "Agent".to_owned(),
                origin: protocol::AgentOrigin::User,
                backend_kind: protocol::BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: None,
                custom_agent_id: None,
                created_at_ms: 0,
                instance_stream: protocol::StreamPath("/agent/agent-1/inst".to_owned()),
                started: true,
                fatal_error: None,
            }]);
            state.hold_submission(PendingSubmission {
                local_submission_id: LocalSubmissionId(FIXTURE_ID),
                origin: SubmissionOriginId(FIXTURE_ORIGIN + 1),
                local_host_id: host,
                connection_instance_id: 7,
                target: SubmissionTarget::Agent(agent_for_mount.clone()),
                text: "no pictures".to_owned(),
                images: Vec::new(),
                tool_response: None,
                state: PendingSubmissionState::NotSent,
            });
            provide_context(state);
            view! { <AgentPendingSubmissions agent_ref=agent_for_mount.clone() /> }
        });
        next_tick().await;

        let resend: HtmlElement = find(&container, "pending-submission-resend")
            .unwrap()
            .dyn_into()
            .unwrap();
        resend.click();
        next_tick().await;
        next_tick().await;

        let lines = crate::bridge::test_sent_lines();
        assert_eq!(lines.len(), 1);
        let frame: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert!(
            frame.pointer("/payload/images").is_none(),
            "an image-less message must omit the field, exactly as a first send does — \
             an empty list is different bytes: {}",
            lines[0]
        );
    }

    /// A long, unbroken message must not widen the page.    /// A long, unbroken message must not widen the page. Horizontal scroll on a
    /// phone hides the right-hand edge of the controls, which here means hiding
    /// the buttons that recover the user's text.
    #[wasm_bindgen_test]
    async fn a_long_message_wraps_instead_of_scrolling_the_page_sideways() {
        let container = make_container();
        let handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let handle_for_mount = handle.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let host = LocalHostId(HOST.to_owned());
            state.active_local_host_id.set(Some(host.clone()));
            state.hold_submission(PendingSubmission {
                local_submission_id: LocalSubmissionId(FIXTURE_ID),
                origin: SubmissionOriginId(FIXTURE_ORIGIN + 4),
                local_host_id: host,
                connection_instance_id: 7,
                target: SubmissionTarget::NewChat,
                // No spaces: nothing for the browser to break on unless the CSS
                // says it may.
                text: "z".repeat(400),
                images: Vec::new(),
                tool_response: None,
                state: PendingSubmissionState::DeliveryUnknown,
            });
            *handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <PendingSubmissions /> }
        });
        next_tick().await;

        let surface: HtmlElement = find(&container, "pending-submissions")
            .unwrap()
            .dyn_into()
            .unwrap();
        let text: HtmlElement = find(&container, "pending-submission-text")
            .unwrap()
            .dyn_into()
            .unwrap();

        // Asserted on the element's own box, because these tests mount into a
        // bare DOM with no stylesheet — which is exactly why the wrapping rule is
        // inline. If it lived only in `styles.css` this assertion would pass
        // vacuously and guard nothing.
        assert!(
            text.scroll_width() <= text.client_width() + 1,
            "an unbroken 400-character message must wrap inside its own box: \
             scroll_width {} vs client_width {}",
            text.scroll_width(),
            text.client_width()
        );
        assert!(
            surface.scroll_width() <= surface.client_width() + 1,
            "the surface must not scroll sideways, or the recovery controls slide \
             off the edge of the phone: scroll_width {} vs client_width {}",
            surface.scroll_width(),
            surface.client_width()
        );
    }

    /// Edit is the explicit "give me my text back" action. It is the only way
    /// the recovery surface may ever write the composer.
    #[wasm_bindgen_test]
    async fn edit_returns_the_text_to_an_empty_composer_and_retires_the_record() {
        let (container, state) = mount_with(PendingSubmissionState::DeliveryUnknown, Some(8)).await;

        let edit: HtmlElement = find(&container, "pending-submission-edit")
            .unwrap()
            .dyn_into()
            .unwrap();
        edit.click();
        next_tick().await;

        assert_eq!(
            state.chat_input.get_untracked(),
            "deploy the thing",
            "Edit must put the exact text back in the composer"
        );
        assert!(
            state.pending_submissions.get_untracked().is_empty(),
            "once the text is back with the user there is nothing left to recover"
        );
    }

    /// The composer belongs to the user. A record must never overwrite what they
    /// are in the middle of typing — not even when they ask for it.
    #[wasm_bindgen_test]
    async fn edit_refuses_to_clobber_a_composer_the_user_is_using() {
        let (container, state) = mount_with(PendingSubmissionState::DeliveryUnknown, Some(8)).await;
        state.chat_input.set("half a thought".to_owned());
        next_tick().await;

        let edit: HtmlElement = find(&container, "pending-submission-edit")
            .unwrap()
            .dyn_into()
            .unwrap();
        edit.click();
        next_tick().await;

        assert_eq!(
            state.chat_input.get_untracked(),
            "half a thought",
            "the user's in-progress text must never be overwritten"
        );
        assert_eq!(
            state.pending_submissions.get_untracked().len(),
            1,
            "the record must stay recoverable when it could not be restored"
        );
        assert!(
            state.mobile_shell_error.get_untracked().is_some(),
            "the user must be told why Edit did nothing"
        );
    }
}
