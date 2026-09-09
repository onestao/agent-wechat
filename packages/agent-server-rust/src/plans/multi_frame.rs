//! Multi-frame regression harness and send-safety contracts (F1-F10).
//!
//! Provides deterministic resolution and verification logic for multi-frame
//! WeChat accessibility trees, preventing phantom/ghost composer selection,
//! target misrouting, and unverified sends.

use crate::ia::selectors::query_selector;
use crate::ia::types::{A11yNode, Bounds};
use serde::{Deserialize, Serialize};

// ============================================================================
// Data Types & Diagnostics
// ============================================================================

/// Actionable diagnostic reasons for send planner failures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendPlannerError {
    /// No edit+send pair could be found in the active chat surface.
    ComposerNotFound,
    /// Multiple eligible composers exist with unresolved ambiguity (fail-closed).
    ComposerAmbiguous { count: usize },
    /// Active chat does not match intended recipient (wrong-target prevention).
    TargetNotVerified {
        expected: String,
        actual: Option<String>,
    },
    /// Search result selection ambiguous or yielded no exact match.
    SearchAmbiguous {
        query: String,
        candidates: Vec<String>,
    },
    /// Search target not found in candidate rows.
    SearchTargetNotFound { query: String },
}

impl std::fmt::Display for SendPlannerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ComposerNotFound => write!(f, "composer_not_found"),
            Self::ComposerAmbiguous { count } => {
                write!(f, "composer_ambiguous:found_{}_candidates", count)
            }
            Self::TargetNotVerified { expected, actual } => write!(
                f,
                "target_not_verified:expected='{}',actual='{}'",
                expected,
                actual.as_deref().unwrap_or("none")
            ),
            Self::SearchAmbiguous { query, candidates } => write!(
                f,
                "search_ambiguous:query='{}',candidates={:?}",
                query, candidates
            ),
            Self::SearchTargetNotFound { query } => {
                write!(f, "search_target_not_found:query='{}'", query)
            }
        }
    }
}

/// Information about a candidate edit+send pair in the a11y tree.
#[derive(Debug, Clone)]
pub struct ComposerCandidate<'a> {
    pub edit_node: &'a A11yNode,
    pub send_node: &'a A11yNode,
    pub frame_name: String,
    pub frame_is_active: bool,
    pub is_main_wechat_frame: bool,
    pub edit_is_focused: bool,
    pub send_is_enabled: bool,
}

/// Information about a top-level frame in the a11y tree.
#[derive(Debug, Clone)]
pub struct FrameInfo<'a> {
    pub node: &'a A11yNode,
    pub name: String,
    pub is_active: bool,
    pub is_main_wechat: bool,
    pub bounds: Option<Bounds>,
}

// ============================================================================
// Frame & Composer Resolvers
// ============================================================================

/// Check if an A11yNode has a specific state (e.g. "ACTIVE", "EDITABLE", "FOCUSED", "DISABLED").
pub fn node_has_state(node: &A11yNode, state: &str) -> bool {
    node.states
        .as_ref()
        .map(|s| s.iter().any(|st| st.eq_ignore_ascii_case(state)))
        .unwrap_or(false)
}

/// Determine if a frame name represents a WeChat chat window (main or detached chat).
pub fn is_main_wechat_frame_name(name: &str) -> bool {
    let lower = name.trim().to_lowercase();
    lower == "wechat"
        || lower == "weixin"
        || lower == "微信"
        || lower.starts_with("wechat - ")
        || lower.starts_with("weixin - ")
        || lower.starts_with("微信 - ")
}

/// Determine if a frame name is a known auxiliary/non-chat frame (e.g. Settings, WeChat Team).
pub fn is_auxiliary_or_stale_frame(name: &str) -> bool {
    let lower = name.trim().to_lowercase();
    lower.contains("settings")
        || lower.contains("设置")
        || lower.contains("wechat team")
        || lower.contains("微信团队")
        || lower.contains("weixin team")
}

/// Collect all top-level frames under the desktop/application root.
pub fn collect_top_level_frames<'a>(root: &'a A11yNode) -> Vec<FrameInfo<'a>> {
    let mut frames = Vec::new();
    find_frames_recursive(root, &mut frames);
    frames
}

fn find_frames_recursive<'a>(node: &'a A11yNode, out: &mut Vec<FrameInfo<'a>>) {
    if node.role == "frame" {
        let name = node.name.clone();
        let is_active = node_has_state(node, "ACTIVE");
        let is_main = is_main_wechat_frame_name(&name);
        out.push(FrameInfo {
            node,
            name,
            is_active,
            is_main_wechat: is_main,
            bounds: node.bounds.clone(),
        });
        return;
    }

    if let Some(children) = &node.children {
        for child in children {
            find_frames_recursive(child, out);
        }
    }
}

/// Collect all candidate edit+send pairs from an a11y subtree.
pub fn collect_composer_pairs<'a>(
    root: &'a A11yNode,
    current_frame: Option<&FrameInfo<'a>>,
    out: &mut Vec<ComposerCandidate<'a>>,
) {
    // If this node is a frame, track it as the current enclosing frame
    let frame_info: Option<FrameInfo<'a>> = if root.role == "frame" {
        let name = root.name.clone();
        let is_active = node_has_state(root, "ACTIVE");
        let is_main = is_main_wechat_frame_name(&name);
        Some(FrameInfo {
            node: root,
            name,
            is_active,
            is_main_wechat: is_main,
            bounds: root.bounds.clone(),
        })
    } else {
        None
    };

    let effective_frame = frame_info.as_ref().or(current_frame);

    if let Some(children) = &root.children {
        // Look for send button
        let send_btn = children.iter().find(|c| {
            c.role == "push-button" && crate::ia::selectors::is_send_button_name(&c.name)
        });

        // Look for editable text node
        let edit_node = children
            .iter()
            .find(|c| c.role == "text" && node_has_state(c, "EDITABLE"));

        if let (Some(edit), Some(send)) = (edit_node, send_btn) {
            let (frame_name, frame_is_active, is_main) = match effective_frame {
                Some(f) => (f.name.clone(), f.is_active, f.is_main_wechat),
                None => ("unknown".to_string(), false, false),
            };

            out.push(ComposerCandidate {
                edit_node: edit,
                send_node: send,
                frame_name,
                frame_is_active,
                is_main_wechat_frame: is_main,
                edit_is_focused: node_has_state(edit, "FOCUSED"),
                send_is_enabled: !node_has_state(send, "DISABLED"),
            });
        }

        // Recurse into children
        for child in children {
            collect_composer_pairs(child, effective_frame, out);
        }
    }
}

/// Resolve the active composer pair with strict multi-frame validation (F1-F5, F9, F10).
///
/// Rules:
/// 1. If no composer candidate exists in the entire tree -> `ComposerNotFound`.
/// 2. If candidates exist under an ACTIVE main WeChat frame, candidates in stale/auxiliary/inactive frames are rejected.
/// 3. If multiple candidates remain in active/eligible chat surfaces without clear priority -> FAIL CLOSED (`ComposerAmbiguous`).
/// 4. If exactly one candidate is eligible -> return the valid `(edit, send)` pair.
pub fn resolve_active_composer<'a>(
    root: &'a A11yNode,
) -> Result<(&'a A11yNode, &'a A11yNode), SendPlannerError> {
    let mut candidates = Vec::new();
    collect_composer_pairs(root, None, &mut candidates);

    if candidates.is_empty() {
        return Err(SendPlannerError::ComposerNotFound);
    }

    // Filter 1: Prefer candidates in frames marked ACTIVE
    let active_candidates: Vec<ComposerCandidate<'a>> = candidates
        .iter()
        .filter(|c| c.frame_is_active)
        .cloned()
        .collect();

    let eligible_pool = if !active_candidates.is_empty() {
        active_candidates
    } else {
        candidates
    };

    // Filter 2: Filter out known auxiliary/stale frames if any main WeChat candidate exists
    let main_frame_candidates: Vec<ComposerCandidate<'a>> = eligible_pool
        .iter()
        .filter(|c| c.is_main_wechat_frame && !is_auxiliary_or_stale_frame(&c.frame_name))
        .cloned()
        .collect();

    let candidates_after_frame_filter = if !main_frame_candidates.is_empty() {
        main_frame_candidates
    } else {
        eligible_pool
            .into_iter()
            .filter(|c| !is_auxiliary_or_stale_frame(&c.frame_name))
            .collect()
    };

    if candidates_after_frame_filter.is_empty() {
        return Err(SendPlannerError::ComposerNotFound);
    }

    // Exactly one winner?
    if candidates_after_frame_filter.len() == 1 {
        let winner = &candidates_after_frame_filter[0];
        return Ok((winner.edit_node, winner.send_node));
    }

    // Multiple candidates in eligible pool: check if one is uniquely focused or has active text
    let focused_candidates: Vec<&ComposerCandidate<'a>> = candidates_after_frame_filter
        .iter()
        .filter(|c| c.edit_is_focused)
        .collect();

    if focused_candidates.len() == 1 {
        return Ok((
            focused_candidates[0].edit_node,
            focused_candidates[0].send_node,
        ));
    }

    // If still ambiguous between >=2 active/eligible composers, fail closed!
    Err(SendPlannerError::ComposerAmbiguous {
        count: candidates_after_frame_filter.len(),
    })
}

// ============================================================================
// Target Verification Contracts (F6, F7)
// ============================================================================

/// Extract the active chat title / header label from the chat surface.
pub fn extract_open_chat_title(root: &A11yNode) -> Option<String> {
    let chat_list = query_selector(root, r#"list[name="Chats"]"#);
    let chat_list_right = chat_list
        .and_then(|c| c.bounds.as_ref())
        .map(|b| b.x + b.width)
        .unwrap_or(272.0);

    let mut all_labels = Vec::new();
    collect_labels_recursive(root, &mut all_labels);

    let header_label = all_labels.iter().find(|label| {
        if let Some(b) = &label.bounds {
            b.x >= chat_list_right
                && b.y < 80.0
                && !label.name.trim().is_empty()
                && !label.name.contains("Send")
                && !label.name.contains("WeChat")
                && !label.name.contains("Weixin")
        } else {
            false
        }
    });

    header_label.map(|l| clean_chat_title(&l.name))
}

fn collect_labels_recursive<'a>(node: &'a A11yNode, out: &mut Vec<&'a A11yNode>) {
    if node.role == "label" && !node.name.is_empty() && node.bounds.is_some() {
        out.push(node);
    }
    if let Some(children) = &node.children {
        for child in children {
            collect_labels_recursive(child, out);
        }
    }
}

/// Strip group member count suffix like "(3)" or " (42)" from chat title.
pub fn clean_chat_title(name: &str) -> String {
    let re = regex::Regex::new(r"\s*\(\d+\)$").unwrap();
    re.replace(name.trim(), "").trim().to_string()
}

/// Determine if the expected chat target matches the actual open chat title,
/// taking into account normalized names and system aliases (e.g. filehelper <-> File Transfer / 文件传输助手).
pub fn is_target_chat_name_match(expected: &str, actual: &str) -> bool {
    let exp = expected.trim().to_lowercase();
    let act = actual.trim().to_lowercase();
    if exp == act {
        return true;
    }
    // filehelper special mapping across English/Chinese locales
    let is_exp_filehelper = exp == "filehelper" || exp == "file transfer" || exp == "文件传输助手";
    let is_act_filehelper = act == "filehelper" || act == "file transfer" || act == "文件传输助手";
    if is_exp_filehelper && is_act_filehelper {
        return true;
    }
    false
}

/// Verify if the active chat matches the target chat (F6, F7).
///
/// Returns:
/// - `Ok(true)` if verified and already open (F7: proceed without Escape/search).
/// - `Err(SendPlannerError::TargetNotVerified)` if chat is open to someone else (F6: fail closed).
/// - `Ok(false)` if no chat is currently open (opening required).
pub fn verify_target_chat(
    root: &A11yNode,
    expected_target: &str,
) -> Result<bool, SendPlannerError> {
    let current_title = extract_open_chat_title(root);

    match current_title {
        Some(actual) => {
            if is_target_chat_name_match(expected_target, &actual) {
                Ok(true) // Target already verified open
            } else {
                Err(SendPlannerError::TargetNotVerified {
                    expected: expected_target.to_string(),
                    actual: Some(actual),
                })
            }
        }
        None => Ok(false), // No chat open
    }
}

// ============================================================================
// Search Matching Contracts (F8)
// ============================================================================

/// Denylist check for system accounts that must never be blindly messaged.
pub fn is_denied_system_chat(name: &str) -> bool {
    let lower = name.trim().to_lowercase();
    lower == "file transfer"
        || lower == "文件传输助手"
        || lower == "微信团队"
        || lower == "wechat team"
        || lower == "weixin team"
}

/// Match search result candidate rows against target name (F8).
///
/// Returns exact matched row name, or fails closed on ambiguity / no match.
pub fn match_search_row<'a>(
    candidate_rows: &[&'a str],
    target: &str,
) -> Result<&'a str, SendPlannerError> {
    let norm_target = target.trim().to_lowercase();
    if norm_target.is_empty() {
        return Err(SendPlannerError::SearchTargetNotFound {
            query: target.to_string(),
        });
    }

    // Filter system accounts
    let filtered_rows: Vec<&'a str> = candidate_rows
        .iter()
        .copied()
        .filter(|row| !is_denied_system_chat(row))
        .collect();

    // Exact matches
    let exact_matches: Vec<&'a str> = filtered_rows
        .iter()
        .copied()
        .filter(|row| row.trim().to_lowercase() == norm_target)
        .collect();

    if exact_matches.len() == 1 {
        return Ok(exact_matches[0]);
    }

    if exact_matches.len() > 1 {
        return Err(SendPlannerError::SearchAmbiguous {
            query: target.to_string(),
            candidates: exact_matches.into_iter().map(String::from).collect(),
        });
    }

    // Check partial matches for diagnostic reporting
    let partial_matches: Vec<&'a str> = filtered_rows
        .iter()
        .copied()
        .filter(|row| row.trim().to_lowercase().contains(&norm_target))
        .collect();

    if partial_matches.len() > 1 {
        return Err(SendPlannerError::SearchAmbiguous {
            query: target.to_string(),
            candidates: partial_matches.into_iter().map(String::from).collect(),
        });
    }

    Err(SendPlannerError::SearchTargetNotFound {
        query: target.to_string(),
    })
}

// ============================================================================
// Multi-Frame Regression Tests (F1 - F10)
// ============================================================================

#[cfg(test)]
pub mod tests {
    use super::*;

    fn load_fixture(name: &str) -> A11yNode {
        let json = match name {
            "f1_clean_single_composer.json" => {
                include_str!("test_fixtures/f1_clean_single_composer.json")
            }
            "f2_active_wechat_stale_settings.json" => {
                include_str!("test_fixtures/f2_active_wechat_stale_settings.json")
            }
            "f3_active_wechat_stale_wechat_team.json" => {
                include_str!("test_fixtures/f3_active_wechat_stale_wechat_team.json")
            }
            "f4_live_multi_frame_observed.json" => {
                include_str!("test_fixtures/f4_live_multi_frame_observed.json")
            }
            "f5_two_active_composers.json" => {
                include_str!("test_fixtures/f5_two_active_composers.json")
            }
            "f6_target_not_verified.json" => {
                include_str!("test_fixtures/f6_target_not_verified.json")
            }
            "f7_target_already_open.json" => {
                include_str!("test_fixtures/f7_target_already_open.json")
            }
            "f8_search_result_ambiguity.json" => {
                include_str!("test_fixtures/f8_search_result_ambiguity.json")
            }
            "f9_popup_auxiliary_present.json" => {
                include_str!("test_fixtures/f9_popup_auxiliary_present.json")
            }
            "f10_no_composer.json" => {
                include_str!("test_fixtures/f10_no_composer.json")
            }
            _ => panic!("Unknown fixture: {name}"),
        };
        serde_json::from_str(json).expect("Fixture JSON should deserialize into A11yNode")
    }

    /// F1: Clean single composer
    /// - WeChat ACTIVE frame
    /// - 1 EDITABLE composer
    /// - 1 Send button
    /// -> Valid pair selected deterministically.
    #[test]
    fn test_f1_clean_single_composer() {
        let a11y = load_fixture("f1_clean_single_composer.json");
        let result = resolve_active_composer(&a11y);
        assert!(result.is_ok(), "F1 must select valid pair");
        let (edit, send) = result.unwrap();
        assert_eq!(edit.role, "text");
        assert!(node_has_state(edit, "EDITABLE"));
        assert_eq!(send.name, "Send(S)");
    }

    /// F2: Active WeChat + stale Settings frame
    /// - WeChat ACTIVE, Settings non-active
    /// - Multiple editable, multiple buttons
    /// -> Only composer belonging to active WeChat chat surface is eligible.
    #[test]
    fn test_f2_active_wechat_stale_settings() {
        let a11y = load_fixture("f2_active_wechat_stale_settings.json");
        let frames = collect_top_level_frames(&a11y);
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().any(|f| f.name == "WeChat" && f.is_active));
        assert!(frames.iter().any(|f| f.name == "Settings" && !f.is_active));

        let result = resolve_active_composer(&a11y);
        assert!(result.is_ok(), "F2 must select WeChat composer");
        let (edit, send) = result.unwrap();
        assert_eq!(edit.role, "text");
        assert!(edit.bounds.as_ref().unwrap().x > 250.0); // Inside WeChat ChatArea
        assert_eq!(send.name, "Send(S)");
    }

    /// F3: Active WeChat + stale WeChat Team frame
    /// - Active WeChat + stale WeChat Team (auxiliary frame)
    /// -> Stale auxiliary frame cannot win composer selection.
    #[test]
    fn test_f3_active_wechat_stale_wechat_team() {
        let a11y = load_fixture("f3_active_wechat_stale_wechat_team.json");
        let mut candidates = Vec::new();
        collect_composer_pairs(&a11y, None, &mut candidates);
        assert_eq!(candidates.len(), 2, "Two candidate pairs exist in tree");

        let result = resolve_active_composer(&a11y);
        assert!(result.is_ok(), "F3 must resolve active WeChat composer");
        let (edit, _) = result.unwrap();
        // Candidate from WeChat has bounds x=280; auxiliary has x=140
        assert_eq!(edit.bounds.as_ref().unwrap().x, 280.0);
    }

    /// F4: Active WeChat + Settings + WeChat Team (Observed Live Condition)
    /// - Observed live: frames >= 3, send >= 2, editable >= 3
    /// -> Deterministic active chat composer selected.
    #[test]
    fn test_f4_live_multi_frame_observed() {
        let a11y = load_fixture("f4_live_multi_frame_observed.json");
        let frames = collect_top_level_frames(&a11y);
        assert!(frames.len() >= 3, "Must model >= 3 frames");

        let mut candidates = Vec::new();
        collect_composer_pairs(&a11y, None, &mut candidates);
        assert!(candidates.len() >= 2, "Must model >= 2 Send buttons");

        let result = resolve_active_composer(&a11y);
        assert!(
            result.is_ok(),
            "F4 must deterministically resolve active chat composer"
        );
        let (edit, send) = result.unwrap();
        assert_eq!(edit.bounds.as_ref().unwrap().x, 280.0);
        assert_eq!(send.bounds.as_ref().unwrap().x, 880.0);
    }

    /// F5: Two apparently active/eligible composers
    /// -> Fail closed, no typing action, diagnostic: composer_ambiguous.
    #[test]
    fn test_f5_two_active_composers_fail_closed() {
        let a11y = load_fixture("f5_two_active_composers.json");
        let result = resolve_active_composer(&a11y);
        assert!(
            result.is_err(),
            "F5 must fail closed when two composers are active"
        );
        match result.unwrap_err() {
            SendPlannerError::ComposerAmbiguous { count } => {
                assert_eq!(count, 2);
            }
            other => panic!("Expected ComposerAmbiguous, got {:?}", other),
        }
    }

    /// F6: Composer exists but target chat cannot be verified
    /// -> Fail closed, diagnostic: target_not_verified.
    #[test]
    fn test_f6_target_not_verified_fail_closed() {
        let a11y = load_fixture("f6_target_not_verified.json");
        // Active chat in fixture is "Alice", but we want to send to "Bob"
        let verified = verify_target_chat(&a11y, "Bob");
        assert!(
            verified.is_err(),
            "F6 must fail closed when target chat does not match"
        );
        match verified.unwrap_err() {
            SendPlannerError::TargetNotVerified { expected, actual } => {
                assert_eq!(expected, "Bob");
                assert_eq!(actual, Some("Alice".to_string()));
            }
            other => panic!("Expected TargetNotVerified, got {:?}", other),
        }
    }

    /// F7: Target already open
    /// -> Verified, no destructive Escape/search sequence, preserve chat.
    #[test]
    fn test_f7_target_already_open() {
        let a11y = load_fixture("f7_target_already_open.json");
        // Target is "Bob" and chat is already open to "Bob"
        let verified = verify_target_chat(&a11y, "Bob");
        assert!(verified.is_ok(), "F7 must verify target is already open");
        assert_eq!(verified.unwrap(), true);

        // Composer is immediately available without searching
        let composer = resolve_active_composer(&a11y);
        assert!(composer.is_ok());
    }

    /// F8: Search result ambiguity
    /// -> Exact target matching; no first-row blind selection; fails closed on ambiguity.
    #[test]
    fn test_f8_search_result_ambiguity() {
        let rows = vec!["File Transfer", "Team Project Alpha", "Team Project Beta"];

        // Query "Team Project" matches both Alpha and Beta -> must fail closed (ambiguous)
        let result_ambig = match_search_row(&rows, "Team Project");
        assert!(result_ambig.is_err());
        match result_ambig.unwrap_err() {
            SendPlannerError::SearchAmbiguous { query, candidates } => {
                assert_eq!(query, "Team Project");
                assert_eq!(candidates.len(), 2);
            }
            other => panic!("Expected SearchAmbiguous, got {:?}", other),
        }

        // Blind selection of first row ("File Transfer") must be rejected
        let system_match = match_search_row(&rows, "File Transfer");
        assert!(
            system_match.is_err(),
            "System accounts like File Transfer must never be matched"
        );

        // Exact match for "Team Project Alpha" succeeds
        let exact = match_search_row(&rows, "Team Project Alpha");
        assert_eq!(exact.unwrap(), "Team Project Alpha");
    }

    /// F9: Popup/auxiliary frame present but known-safe to ignore
    /// -> Planner remains deterministic and selects active composer.
    #[test]
    fn test_f9_popup_auxiliary_present_deterministic() {
        let a11y = load_fixture("f9_popup_auxiliary_present.json");
        let result = resolve_active_composer(&a11y);
        assert!(
            result.is_ok(),
            "F9 must remain deterministic with safe auxiliary element"
        );
        let (edit, send) = result.unwrap();
        assert_eq!(edit.role, "text");
        assert_eq!(send.name, "Send(S)");
    }

    /// F10: No composer
    /// -> Fail closed with explicit diagnostic reason: composer_not_found.
    #[test]
    fn test_f10_no_composer_fail_closed() {
        let a11y = load_fixture("f10_no_composer.json");
        let result = resolve_active_composer(&a11y);
        assert!(
            result.is_err(),
            "F10 must fail closed when no composer exists"
        );
        match result.unwrap_err() {
            SendPlannerError::ComposerNotFound => {}
            other => panic!("Expected ComposerNotFound, got {:?}", other),
        }
    }
}
