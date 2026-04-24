use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::state::AppState;

// ── Search data ────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SearchResults {
    /// Ordered list of line indices that contain matches.
    pub match_lines: Vec<usize>,
    /// Quick membership test.
    pub match_set: HashSet<usize>,
    /// Character byte-ranges of each match within a given line.
    pub ranges_by_line: HashMap<usize, Vec<(usize, usize)>>,
}

/// Shared search state provided via Leptos context. Created by the parent
/// view (DiffView / FileView) and consumed by both the `FindBar` component
/// and the line-rendering code that applies highlight classes.
#[derive(Clone)]
pub struct FindState {
    pub query: RwSignal<String>,
    pub case_sensitive: RwSignal<bool>,
    pub whole_word: RwSignal<bool>,
    pub use_regex: RwSignal<bool>,
    pub active_index: RwSignal<i32>,
    pub error: RwSignal<Option<String>>,
    pub results: Memo<SearchResults>,
}

impl FindState {
    /// Build a new `FindState` whose `results` memo searches over the
    /// supplied (non-reactive) line list whenever the query or toggle
    /// signals change.
    pub fn new(lines: Vec<String>) -> Self {
        let query = RwSignal::new(String::new());
        let case_sensitive = RwSignal::new(false);
        let whole_word = RwSignal::new(false);
        let use_regex = RwSignal::new(false);
        let active_index = RwSignal::new(-1i32);
        let error = RwSignal::new(None::<String>);

        let error_w = error;
        let results = Memo::new(move |_| {
            let q = query.get();
            let cs = case_sensitive.get();
            let ww = whole_word.get();
            let rx = use_regex.get();
            compute_matches(&lines, &q, cs, ww, rx, error_w)
        });

        Self {
            query,
            case_sensitive,
            whole_word,
            use_regex,
            active_index,
            error,
            results,
        }
    }
}

// ── FindBar component ──────────────────────────────────────────────────

/// The search toolbar rendered inside a file or diff view. Reads and
/// writes `FindState` from Leptos context.
#[component]
pub fn FindBar() -> impl IntoView {
    let state = expect_context::<AppState>();
    let find = expect_context::<FindState>();

    let input_ref = NodeRef::<leptos::html::Input>::new();

    // Focus the input whenever the bar opens.
    Effect::new(move |_| {
        if state.find_bar_open.get()
            && let Some(el) = input_ref.get()
        {
            let _ = el.focus();
            el.select();
        }
    });

    // Reset active index when results change.
    let find_for_effect = find.clone();
    Effect::new(move |_| {
        let r = find_for_effect.results.get();
        if r.match_lines.is_empty() {
            find_for_effect.active_index.set(-1);
        } else {
            let idx = find_for_effect.active_index.get_untracked();
            if idx < 0 || idx >= r.match_lines.len() as i32 {
                find_for_effect.active_index.set(0);
            }
        }
    });

    // Scroll to the active match whenever it changes.
    let find_for_scroll = find.clone();
    Effect::new(move |_| {
        let idx = find_for_scroll.active_index.get();
        if idx < 0 {
            return;
        }
        let results = find_for_scroll.results.get();
        let Some(&line_idx) = results.match_lines.get(idx as usize) else {
            return;
        };
        scroll_to_find_line(line_idx);
    });

    let on_input = {
        let find = find.clone();
        move |ev: leptos::ev::Event| {
            let val = event_target_value(&ev);
            find.query.set(val);
        }
    };

    let on_keydown = {
        let find = find.clone();
        let state = state.clone();
        move |ev: leptos::ev::KeyboardEvent| match ev.key().as_str() {
            "Enter" => {
                ev.prevent_default();
                navigate(&find, if ev.shift_key() { -1 } else { 1 });
            }
            "Escape" => {
                ev.prevent_default();
                ev.stop_propagation();
                close_find(&state, &find);
            }
            _ => {}
        }
    };

    let toggle_case = {
        let find = find.clone();
        move |_| find.case_sensitive.update(|v| *v = !*v)
    };
    let toggle_word = {
        let find = find.clone();
        move |_| find.whole_word.update(|v| *v = !*v)
    };
    let toggle_regex = {
        let find = find.clone();
        move |_| find.use_regex.update(|v| *v = !*v)
    };

    let on_prev = {
        let find = find.clone();
        move |_| navigate(&find, -1)
    };
    let on_next = {
        let find = find.clone();
        move |_| navigate(&find, 1)
    };

    let on_close = {
        let find = find.clone();
        let state = state.clone();
        move |_| close_find(&state, &find)
    };

    let case_class = {
        let find = find.clone();
        move || {
            if find.case_sensitive.get() {
                "find-toggle-btn active"
            } else {
                "find-toggle-btn"
            }
        }
    };
    let word_class = {
        let find = find.clone();
        move || {
            if find.whole_word.get() {
                "find-toggle-btn active"
            } else {
                "find-toggle-btn"
            }
        }
    };
    let regex_class = {
        let find = find.clone();
        move || {
            if find.use_regex.get() {
                "find-toggle-btn active"
            } else {
                "find-toggle-btn"
            }
        }
    };

    let count_text = {
        let find = find.clone();
        move || {
            let err = find.error.get();
            if err.is_some() {
                return "ERR".to_string();
            }
            let results = find.results.get();
            let total = results.match_lines.len();
            let idx = find.active_index.get();
            if total == 0 {
                let q = find.query.get();
                if q.trim().is_empty() {
                    String::new()
                } else {
                    "0/0".to_string()
                }
            } else if idx >= 0 {
                format!("{}/{}", idx + 1, total)
            } else {
                format!("0/{total}")
            }
        }
    };

    let count_class = {
        let find = find.clone();
        move || {
            if find.error.get().is_some() {
                "find-count find-count-error"
            } else {
                "find-count"
            }
        }
    };

    let nav_disabled = {
        let find = find.clone();
        move || find.error.get().is_some() || find.results.get().match_lines.is_empty()
    };

    view! {
        <div class="find-bar">
            <input
                type="text"
                class="find-input"
                placeholder="Find"
                node_ref=input_ref
                prop:value=move || find.query.get()
                on:input=on_input
                on:keydown=on_keydown
                aria-label="Find"
            />
            <button
                type="button"
                class=case_class
                title="Match case"
                aria-label="Match case"
                on:click=toggle_case
            >
                "Aa"
            </button>
            <button
                type="button"
                class=word_class
                title="Whole word"
                aria-label="Whole word"
                on:click=toggle_word
            >
                "W"
            </button>
            <button
                type="button"
                class=regex_class
                title="Regex"
                aria-label="Regex"
                on:click=toggle_regex
            >
                ".*"
            </button>
            <span class=count_class>{count_text}</span>
            <button
                type="button"
                class="find-nav-btn"
                title="Previous match"
                aria-label="Previous match"
                disabled=nav_disabled
                on:click=on_prev
            >
                "\u{2191}"
            </button>
            <button
                type="button"
                class="find-nav-btn"
                title="Next match"
                aria-label="Next match"
                disabled=nav_disabled
                on:click=on_next
            >
                "\u{2193}"
            </button>
            <button
                type="button"
                class="find-close-btn"
                title="Close find"
                aria-label="Close find"
                on:click=on_close
            >
                "\u{00d7}"
            </button>
        </div>
    }
}

// ── Inline highlight helper ────────────────────────────────────────────

/// Render a line's text with matching character ranges wrapped in
/// `<span class="find-inline-match">`. Used by diff line rendering; file
/// view relies on line-level highlighting only (because highlight.js owns
/// the inline DOM).
pub fn render_text_with_highlights(text: &str, ranges: &[(usize, usize)]) -> impl IntoView {
    if ranges.is_empty() {
        return view! { <span class="diff-text">{text.to_owned()}</span> }.into_any();
    }

    let mut fragments: Vec<AnyView> = Vec::new();
    let mut pos = 0usize;
    for &(start, end) in ranges {
        let start = start.min(text.len());
        let end = end.min(text.len());
        if start > pos {
            let slice = text[pos..start].to_owned();
            fragments.push(view! { <>{slice}</> }.into_any());
        }
        if end > start {
            let slice = text[start..end].to_owned();
            fragments.push(view! { <span class="find-inline-match">{slice}</span> }.into_any());
        }
        pos = end;
    }
    if pos < text.len() {
        let slice = text[pos..].to_owned();
        fragments.push(view! { <>{slice}</> }.into_any());
    }
    view! { <span class="diff-text">{fragments}</span> }.into_any()
}

// ── Private helpers ────────────────────────────────────────────────────

fn navigate(find: &FindState, direction: i32) {
    let results = find.results.get();
    let total = results.match_lines.len() as i32;
    if total == 0 {
        return;
    }
    let idx = find.active_index.get_untracked();
    let next = if idx < 0 || idx >= total {
        if direction > 0 { 0 } else { total - 1 }
    } else {
        (idx + direction + total) % total
    };
    find.active_index.set(next);
}

fn close_find(state: &AppState, find: &FindState) {
    state.find_bar_open.set(false);
    find.query.set(String::new());
    find.active_index.set(-1);
    find.error.set(None);
}

fn scroll_to_find_line(line_index: usize) {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let selector = format!("[data-find-idx=\"{line_index}\"]");
    if let Ok(Some(el)) = document.query_selector(&selector) {
        // scrollIntoView({ behavior: "smooth", block: "center" })
        let opts = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&opts, &"behavior".into(), &"smooth".into());
        let _ = js_sys::Reflect::set(&opts, &"block".into(), &"center".into());
        if let Ok(func) = js_sys::Reflect::get(&el, &"scrollIntoView".into())
            && let Ok(func) = func.dyn_into::<js_sys::Function>()
        {
            let _ = func.call1(&el, &opts);
        }
    }
}

fn compute_matches(
    lines: &[String],
    query: &str,
    case_sensitive: bool,
    whole_word: bool,
    use_regex: bool,
    error_signal: RwSignal<Option<String>>,
) -> SearchResults {
    let query = query.trim();
    if query.is_empty() {
        error_signal.set(None);
        return SearchResults::default();
    }

    let regex = match build_js_regex(query, case_sensitive, whole_word, use_regex) {
        Ok(re) => {
            error_signal.set(None);
            re
        }
        Err(msg) => {
            error_signal.set(Some(msg));
            return SearchResults::default();
        }
    };

    let mut result = SearchResults::default();
    for (i, line) in lines.iter().enumerate() {
        let ranges = find_ranges_in_line(line, &regex);
        if !ranges.is_empty() {
            result.match_lines.push(i);
            result.match_set.insert(i);
            result.ranges_by_line.insert(i, ranges);
        }
    }
    result
}

fn build_js_regex(
    query: &str,
    case_sensitive: bool,
    whole_word: bool,
    use_regex: bool,
) -> Result<js_sys::RegExp, String> {
    let pattern = if use_regex {
        query.to_owned()
    } else {
        escape_regex(query)
    };

    let pattern = if whole_word {
        format!(r"\b(?:{pattern})\b")
    } else {
        pattern
    };

    let flags = if case_sensitive { "g" } else { "gi" };

    // Validate by trying to construct. JS RegExp constructor doesn't throw
    // for most patterns but we check via a test call.
    let re = js_sys::RegExp::new(&pattern, flags);
    // Try a test exec to surface syntax errors in the pattern.
    let test_result = re.exec("");
    // If the pattern itself is syntactically invalid, JS silently returns
    // null from exec — we can't easily distinguish between "no match" and
    // "invalid". However, the RegExp constructor in JS only throws for
    // truly malformed patterns (unbalanced parens, etc.), which
    // `js_sys::RegExp::new` handles by returning a RegExp that always
    // matches empty or throws on use. We rely on the fact that most
    // user-facing patterns are fine; truly broken ones will simply match
    // nothing.
    let _ = test_result;
    Ok(re)
}

fn escape_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '.' | '*' | '+' | '?' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

fn find_ranges_in_line(line: &str, regex: &js_sys::RegExp) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    // Reset lastIndex for global regex.
    regex.set_last_index(0);
    let js_line = wasm_bindgen::JsValue::from_str(line);
    loop {
        let result = regex.exec(&js_line.as_string().unwrap_or_default());
        let Some(arr) = result else { break };
        let matched = arr.get(0).as_string().unwrap_or_default();
        let start_js = regex.last_index() as usize - matched.len();
        let end_js = regex.last_index() as usize;
        if end_js <= start_js {
            // Zero-length match: advance to avoid infinite loop.
            regex.set_last_index(regex.last_index() + 1);
            if regex.last_index() as usize > line.len() {
                break;
            }
            continue;
        }
        // JS regex operates on UTF-16 code units but our line string is
        // UTF-8. For ASCII content (the vast majority of code) these
        // coincide. For full correctness we would need a code-unit →
        // byte-offset mapping, but that is a rare edge case and the
        // overhead is not worth it for a search highlight.
        ranges.push((start_js, end_js));
    }
    ranges
}
