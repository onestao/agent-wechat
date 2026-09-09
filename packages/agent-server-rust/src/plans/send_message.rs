use super::Plan;
use crate::db::get_db;
use crate::ia::actions;
use crate::ia::selectors::query_selector;
use crate::ia::types::*;
use crate::plans::multi_frame::{
    node_has_state, resolve_active_composer, resolve_exact_target_row, resolve_search_box,
    verify_target_chat, SendPlannerError,
};
use crate::sessions::manager::get_session;
use crate::tools::chat_select::{open_chat, OpenChatResult};
use crate::tools::exec::{exec_command, ExecOptions};
use crate::tools::wechat_chats;
use crate::tools::wechat_keys::get_stored_keys;

pub struct SendMessagePlan;

pub struct SendMessageParams {
    pub chat_id: String,
    pub message: Option<String>,
    pub image_path: Option<String>,
    pub image_mime: Option<String>,
    pub file_path: Option<String>,
}

pub enum SendMessagePhase {
    Opening,
    VerifyingPrimaryOpen,
    FallbackLocatingTarget,
    FallbackFocusingSearch,
    FallbackSelectingTarget,
    FallbackVerifyingTarget,
    Focusing,
    Inputting,
    Confirming,
    Done,
}

pub struct SendMessagePlanState {
    pub phase: SendMessagePhase,
    pub open_result: Option<OpenChatResult>,
    pub confirm_attempts: u32,
    pub send_action_executed: bool,
    pub failure_reason: Option<String>,
    pub resolved_target_name: Option<String>,
    pub primary_open_error: Option<String>,
}

fn resolve_target_display_name(chat_id: &str) -> Result<String, String> {
    let session = get_session("default").ok_or_else(|| "target_resolve:no_session".to_string())?;
    let logged_in_user = session
        .logged_in_user
        .clone()
        .ok_or_else(|| "target_resolve:not_logged_in".to_string())?;
    let keys = {
        let db = get_db();
        get_stored_keys(&db, &session.id, &logged_in_user)
    };
    if !keys.contains_key("session.db") || !keys.contains_key("contact.db") {
        return Err("target_resolve:missing_session_or_contact_key".to_string());
    }
    let chat = wechat_chats::get_chat_by_username(&logged_in_user, &keys, chat_id)
        .ok_or_else(|| "target_resolve:chat_not_found".to_string())?;
    let name = chat.name.trim();
    if name.is_empty() {
        return Err("target_resolve:display_name_missing".to_string());
    }
    Ok(name.to_string())
}

fn classify_primary_open_error(error: &str) -> &'static str {
    let lower = error.to_lowercase();
    if lower.contains("no sessions found") {
        "session_enumeration_failed"
    } else if lower.contains("unknown buildid") {
        "unknown_build_profile"
    } else if lower.contains("buildid") {
        "build_id_unavailable"
    } else if lower.contains("frida") || lower.contains("auxv-not-found") {
        "frida_attach_failed"
    } else {
        "primary_open_failed"
    }
}

#[async_trait::async_trait]
impl Plan for SendMessagePlan {
    type PlanState = SendMessagePlanState;
    type Params = SendMessageParams;

    fn id(&self) -> &str {
        "send_message"
    }

    fn initial_plan_state(&self) -> SendMessagePlanState {
        SendMessagePlanState {
            phase: SendMessagePhase::Opening,
            open_result: None,
            confirm_attempts: 0,
            send_action_executed: false,
            failure_reason: None,
            resolved_target_name: None,
            primary_open_error: None,
        }
    }

    fn is_goal_reached(&self, _state: &AppState, plan_state: &SendMessagePlanState) -> bool {
        matches!(plan_state.phase, SendMessagePhase::Done)
    }

    fn failure_reason(&self, plan_state: &Self::PlanState) -> Option<String> {
        plan_state.failure_reason.clone()
    }

    async fn select_action(
        &self,
        state: &AppState,
        params: &SendMessageParams,
        identified: &IdentifiedStates,
        plan_state: &mut SendMessagePlanState,
        a11y: &A11yNode,
        _session_id: &str,
    ) -> Option<SelectedAction> {
        let main_state_id = identified.main_window.as_ref().map(|m| m.state_id.as_str());

        // Dismiss popups
        if state.popup.is_some() && identified.popup.is_some() {
            return Some(SelectedAction {
                action: actions::dismiss_popup(),
                frame: identified
                    .main_window
                    .as_ref()
                    .and_then(|m| m.frame.clone()),
            });
        }

        loop {
            match &plan_state.phase {
                SendMessagePhase::Opening => {
                    if main_state_id != Some("chat") && main_state_id != Some("chat_open") {
                        plan_state.failure_reason = Some(format!(
                            "invalid_main_state:{}",
                            main_state_id.unwrap_or("none")
                        ));
                        return None;
                    }

                    if plan_state.resolved_target_name.is_none() {
                        match resolve_target_display_name(&params.chat_id) {
                            Ok(name) => {
                                tracing::info!(
                                    "[send] target identity resolved for chat_id (display_name_len={})",
                                    name.chars().count()
                                );
                                plan_state.resolved_target_name = Some(name);
                            }
                            Err(err) => {
                                tracing::error!(
                                    "[send] target identity resolution failed: {}",
                                    err
                                );
                                plan_state.failure_reason = Some(err);
                                return None;
                            }
                        }
                    }
                    let target_name = plan_state
                        .resolved_target_name
                        .as_deref()
                        .expect("resolved target name must be set");

                    // Port Item 4 & Fixture F7: Verify if desired target is already open.
                    // If positively verified, skip redundant open_chat and proceed to focusing.
                    match verify_target_chat(a11y, target_name) {
                        Ok(true) => {
                            tracing::info!(
                                "[send] target already verified open, proceeding directly to focusing"
                            );
                            plan_state.phase = SendMessagePhase::Focusing;
                            continue;
                        }
                        Err(err) => {
                            tracing::info!(
                                "[send] active chat mismatch ({}), initiating primary open_chat",
                                err
                            );
                        }
                        Ok(false) => {
                            tracing::info!(
                                "[send] no active chat detected, initiating primary open_chat"
                            );
                        }
                    }

                    let chat_list_item = query_selector(a11y, r#"list[name="Chats"] > list-item"#);
                    let click_xy = chat_list_item.and_then(|item| {
                        item.bounds.as_ref().map(|b| {
                            (
                                (b.x + b.width / 2.0).round(),
                                (b.y + b.height / 2.0).round(),
                            )
                        })
                    });

                    let force = main_state_id == Some("chat");
                    let result = open_chat(&params.chat_id, force, click_xy).await;

                    if !result.ok {
                        let err_str = result
                            .error
                            .clone()
                            .unwrap_or_else(|| "open_chat_failed".to_string());
                        let error_class = classify_primary_open_error(&err_str);
                        tracing::warn!(
                            "[send] primary open_chat failed; entering send-safe a11y fallback: class={}",
                            error_class
                        );
                        plan_state.primary_open_error = Some(error_class.to_string());
                        plan_state.open_result = Some(result);
                        plan_state.phase = SendMessagePhase::FallbackLocatingTarget;
                        continue;
                    }

                    let skipped = result.skipped.unwrap_or(false);
                    plan_state.open_result = Some(result);
                    plan_state.phase = SendMessagePhase::VerifyingPrimaryOpen;

                    if !skipped {
                        return Some(SelectedAction {
                            action: actions::wait_short(),
                            frame: identified
                                .main_window
                                .as_ref()
                                .and_then(|m| m.frame.clone()),
                        });
                    }
                    continue;
                }

                SendMessagePhase::VerifyingPrimaryOpen => {
                    let target_name = match plan_state.resolved_target_name.as_deref() {
                        Some(v) => v,
                        None => {
                            plan_state.failure_reason =
                                Some("target_resolve:state_missing".to_string());
                            return None;
                        }
                    };
                    match verify_target_chat(a11y, target_name) {
                        Ok(true) => {
                            plan_state.phase = SendMessagePhase::Focusing;
                            continue;
                        }
                        Ok(false) => {
                            plan_state.failure_reason =
                                Some("primary_open_target_not_verified:none".to_string());
                            return None;
                        }
                        Err(err) => {
                            plan_state.failure_reason =
                                Some(format!("primary_open_target_not_verified:{}", err));
                            return None;
                        }
                    }
                }

                SendMessagePhase::FallbackLocatingTarget => {
                    let target_name = match plan_state.resolved_target_name.as_deref() {
                        Some(v) => v,
                        None => {
                            plan_state.failure_reason =
                                Some("target_resolve:state_missing".to_string());
                            return None;
                        }
                    };

                    // If the target is already visible as a unique row in the
                    // active main WeChat frame, click that exact row directly.
                    match resolve_exact_target_row(a11y, target_name) {
                        Ok(row) => {
                            if let Some(bounds) = &row.bounds {
                                tracing::info!(
                                    "[send] fallback exact visible target row selected; clicking without keyboard Return"
                                );
                                plan_state.phase = SendMessagePhase::FallbackVerifyingTarget;
                                return Some(SelectedAction {
                                    action: actions::sequence(vec![
                                        actions::click_bounds(bounds),
                                        Action::Wait { ms: 500 },
                                    ]),
                                    frame: identified
                                        .main_window
                                        .as_ref()
                                        .and_then(|m| m.frame.clone()),
                                });
                            }
                            plan_state.failure_reason =
                                Some("fallback_target_row_missing_bounds".to_string());
                            return None;
                        }
                        Err(SendPlannerError::SearchAmbiguous { .. }) => {
                            plan_state.failure_reason =
                                Some("fallback_target_row_ambiguous".to_string());
                            return None;
                        }
                        Err(_) => {
                            // Not visible: proceed to a send-safe search path.
                        }
                    }

                    let search = match resolve_search_box(a11y) {
                        Ok(node) => node,
                        Err(err) => {
                            let primary = plan_state
                                .primary_open_error
                                .as_deref()
                                .unwrap_or("unknown");
                            plan_state.failure_reason = Some(format!(
                                "open_chat_primary_failed:{};fallback:{}",
                                primary, err
                            ));
                            return None;
                        }
                    };
                    let bounds = match &search.bounds {
                        Some(b) => b,
                        None => {
                            plan_state.failure_reason =
                                Some("fallback_search_box_missing_bounds".to_string());
                            return None;
                        }
                    };
                    plan_state.phase = SendMessagePhase::FallbackFocusingSearch;
                    return Some(SelectedAction {
                        action: actions::click_bounds(bounds),
                        frame: identified
                            .main_window
                            .as_ref()
                            .and_then(|m| m.frame.clone()),
                    });
                }

                SendMessagePhase::FallbackFocusingSearch => {
                    let target_name = match plan_state.resolved_target_name.as_deref() {
                        Some(v) => v.to_string(),
                        None => {
                            plan_state.failure_reason =
                                Some("target_resolve:state_missing".to_string());
                            return None;
                        }
                    };
                    let search = match resolve_search_box(a11y) {
                        Ok(node) => node,
                        Err(err) => {
                            plan_state.failure_reason = Some(err.to_string());
                            return None;
                        }
                    };
                    if !node_has_state(search, "FOCUSED") {
                        plan_state.failure_reason =
                            Some("fallback_search_box_not_focused".to_string());
                        return None;
                    }

                    // No Return/Enter is ever used in the fallback search path.
                    // Even a focus race therefore cannot accidentally send a
                    // message from a composer.
                    plan_state.phase = SendMessagePhase::FallbackSelectingTarget;
                    return Some(SelectedAction {
                        action: actions::sequence(vec![
                            Action::Key {
                                combo: "ctrl+a".to_string(),
                            },
                            Action::Type {
                                text: target_name,
                                selector: None,
                            },
                            Action::Wait { ms: 700 },
                        ]),
                        frame: identified
                            .main_window
                            .as_ref()
                            .and_then(|m| m.frame.clone()),
                    });
                }

                SendMessagePhase::FallbackSelectingTarget => {
                    let target_name = match plan_state.resolved_target_name.as_deref() {
                        Some(v) => v,
                        None => {
                            plan_state.failure_reason =
                                Some("target_resolve:state_missing".to_string());
                            return None;
                        }
                    };
                    let row = match resolve_exact_target_row(a11y, target_name) {
                        Ok(row) => row,
                        Err(err) => {
                            plan_state.failure_reason = Some(format!("fallback_search:{}", err));
                            return None;
                        }
                    };
                    let bounds = match &row.bounds {
                        Some(b) => b,
                        None => {
                            plan_state.failure_reason =
                                Some("fallback_search_result_missing_bounds".to_string());
                            return None;
                        }
                    };
                    plan_state.phase = SendMessagePhase::FallbackVerifyingTarget;
                    return Some(SelectedAction {
                        action: actions::sequence(vec![
                            actions::click_bounds(bounds),
                            Action::Wait { ms: 500 },
                        ]),
                        frame: identified
                            .main_window
                            .as_ref()
                            .and_then(|m| m.frame.clone()),
                    });
                }

                SendMessagePhase::FallbackVerifyingTarget => {
                    let target_name = match plan_state.resolved_target_name.as_deref() {
                        Some(v) => v,
                        None => {
                            plan_state.failure_reason =
                                Some("target_resolve:state_missing".to_string());
                            return None;
                        }
                    };
                    match verify_target_chat(a11y, target_name) {
                        Ok(true) => {
                            tracing::info!("[send] fallback target positively verified open");
                            plan_state.phase = SendMessagePhase::Focusing;
                            continue;
                        }
                        Ok(false) => {
                            plan_state.failure_reason =
                                Some("fallback_target_not_verified:actual='none'".to_string());
                            return None;
                        }
                        Err(err) => {
                            plan_state.failure_reason =
                                Some(format!("fallback_target_not_verified:{}", err));
                            return None;
                        }
                    }
                }

                SendMessagePhase::Focusing => {
                    if main_state_id != Some("chat_open") {
                        plan_state.failure_reason = Some(format!(
                            "focusing_invalid_main_state:{}",
                            main_state_id.unwrap_or("none")
                        ));
                        return None;
                    }

                    // Port Item 3 & Fixtures F1-F5: Frame-scoped active composer resolution
                    let (edit_node, _) = match resolve_active_composer(a11y) {
                        Ok(pair) => pair,
                        Err(err) => {
                            tracing::warn!(
                                "[send] resolve_active_composer failed in focusing: {}",
                                err
                            );
                            plan_state.failure_reason = Some(err.to_string());
                            return None;
                        }
                    };

                    plan_state.phase = SendMessagePhase::Inputting;

                    let is_focused = node_has_state(edit_node, "FOCUSED");

                    if is_focused {
                        continue;
                    }

                    if let Some(bounds) = &edit_node.bounds {
                        return Some(SelectedAction {
                            action: actions::click_bounds(bounds),
                            frame: identified
                                .main_window
                                .as_ref()
                                .and_then(|m| m.frame.clone()),
                        });
                    }

                    plan_state.failure_reason = Some("edit_node_missing_bounds".to_string());
                    return None;
                }

                SendMessagePhase::Inputting => {
                    // Port Item 2: Single-send idempotency guard against duplicate send
                    if plan_state.send_action_executed {
                        tracing::warn!("[send] send_action_guard_triggered: already executed");
                        plan_state.phase = SendMessagePhase::Done;
                        return None;
                    }

                    let target_name = match plan_state.resolved_target_name.as_deref() {
                        Some(v) => v,
                        None => {
                            plan_state.failure_reason =
                                Some("target_resolve:state_missing".to_string());
                            return None;
                        }
                    };

                    // Port Item 4 & Fixture F6: Positive target verification before typing.
                    // Real send targets are usernames/wxids while the visible
                    // chat header contains the resolved display name, so the
                    // verification must use that resolved identity rather than
                    // comparing a wxid directly to a nickname.
                    match verify_target_chat(a11y, target_name) {
                        Ok(true) => {
                            // Target positively verified open
                        }
                        Err(err) => {
                            tracing::error!(
                                "[send] target verification failed before inputting: {}",
                                err
                            );
                            plan_state.failure_reason = Some(err.to_string());
                            return None;
                        }
                        Ok(false) => {
                            tracing::warn!("[send] target chat header not detected; fail-closed");
                            plan_state.failure_reason = Some("target_not_verified".to_string());
                            return None;
                        }
                    }

                    // Frame-scoped active composer resolution
                    let _pair = match resolve_active_composer(a11y) {
                        Ok(pair) => pair,
                        Err(err) => {
                            tracing::warn!(
                                "[send] resolve_active_composer failed in inputting: {}",
                                err
                            );
                            plan_state.failure_reason = Some(err.to_string());
                            return None;
                        }
                    };

                    plan_state.send_action_executed = true;
                    plan_state.phase = SendMessagePhase::Confirming;

                    // File
                    if let Some(fp) = &params.file_path {
                        exec_command("paste-file", &[fp], &ExecOptions::default()).await;
                        return Some(SelectedAction {
                            action: actions::sequence(vec![
                                Action::Wait { ms: 100 },
                                Action::Key {
                                    combo: "Return".to_string(),
                                },
                            ]),
                            frame: identified
                                .main_window
                                .as_ref()
                                .and_then(|m| m.frame.clone()),
                        });
                    }

                    // Image
                    if let Some(ip) = &params.image_path {
                        let mut args: Vec<&str> = vec![ip];
                        if let Some(mime) = &params.image_mime {
                            args.push(mime);
                        }
                        exec_command("paste-image", &args, &ExecOptions::default()).await;
                        return Some(SelectedAction {
                            action: actions::sequence(vec![
                                Action::Wait { ms: 100 },
                                Action::Key {
                                    combo: "Return".to_string(),
                                },
                            ]),
                            frame: identified
                                .main_window
                                .as_ref()
                                .and_then(|m| m.frame.clone()),
                        });
                    }

                    // Text
                    if let Some(msg) = &params.message {
                        return Some(SelectedAction {
                            action: actions::sequence(vec![
                                Action::Key {
                                    combo: "ctrl+a".to_string(),
                                },
                                Action::Type {
                                    text: msg.clone(),
                                    selector: None,
                                },
                                Action::Wait { ms: 100 },
                                Action::Key {
                                    combo: "Return".to_string(),
                                },
                            ]),
                            frame: identified
                                .main_window
                                .as_ref()
                                .and_then(|m| m.frame.clone()),
                        });
                    }

                    plan_state.failure_reason = Some("no_payload_specified".to_string());
                    return None;
                }

                SendMessagePhase::Confirming => {
                    let target_name = match plan_state.resolved_target_name.as_deref() {
                        Some(v) => v,
                        None => {
                            plan_state.failure_reason =
                                Some("target_resolve:state_missing".to_string());
                            return None;
                        }
                    };
                    match verify_target_chat(a11y, target_name) {
                        Ok(true) => {}
                        Ok(false) => {
                            plan_state.failure_reason =
                                Some("confirm_target_not_verified:actual='none'".to_string());
                            return None;
                        }
                        Err(err) => {
                            plan_state.failure_reason =
                                Some(format!("confirm_target_not_verified:{}", err));
                            return None;
                        }
                    }

                    let (_, send_btn) = match resolve_active_composer(a11y) {
                        Ok(pair) => pair,
                        Err(err) => {
                            tracing::warn!("[send] resolve_active_composer in confirming: {}", err);
                            plan_state.failure_reason = Some(err.to_string());
                            return None;
                        }
                    };

                    let is_disabled = node_has_state(send_btn, "DISABLED");

                    if is_disabled {
                        plan_state.phase = SendMessagePhase::Done;
                        return Some(SelectedAction {
                            action: actions::wait_short(),
                            frame: identified
                                .main_window
                                .as_ref()
                                .and_then(|m| m.frame.clone()),
                        });
                    }

                    plan_state.confirm_attempts += 1;
                    if plan_state.confirm_attempts >= 5 {
                        plan_state.failure_reason =
                            Some("confirm_timeout_send_button_still_enabled".to_string());
                        return None;
                    }

                    return Some(SelectedAction {
                        action: actions::wait_short(),
                        frame: identified
                            .main_window
                            .as_ref()
                            .and_then(|m| m.frame.clone()),
                    });
                }

                SendMessagePhase::Done => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> A11yNode {
        let raw = match name {
            "f4" => include_str!("test_fixtures/f4_live_multi_frame_observed.json"),
            "f6" => include_str!("test_fixtures/f6_target_not_verified.json"),
            _ => panic!("unknown fixture"),
        };
        serde_json::from_str(raw).expect("fixture must parse")
    }

    fn action_contains_return(action: &Action) -> bool {
        match action {
            Action::Key { combo } => {
                combo.eq_ignore_ascii_case("return") || combo.eq_ignore_ascii_case("enter")
            }
            Action::Sequence { actions } => actions.iter().any(action_contains_return),
            _ => false,
        }
    }

    #[test]
    fn test_initial_plan_state_defaults() {
        let plan = SendMessagePlan;
        let state = plan.initial_plan_state();
        assert!(!state.send_action_executed);
        assert!(state.failure_reason.is_none());
        assert_eq!(state.confirm_attempts, 0);
        assert!(matches!(state.phase, SendMessagePhase::Opening));
    }

    #[test]
    fn test_failure_reason_propagation() {
        let plan = SendMessagePlan;
        let mut state = plan.initial_plan_state();
        assert_eq!(plan.failure_reason(&state), None);

        state.failure_reason = Some("composer_ambiguous:found_2_candidates".to_string());
        assert_eq!(
            plan.failure_reason(&state),
            Some("composer_ambiguous:found_2_candidates".to_string())
        );
    }

    #[tokio::test]
    async fn test_send_action_guard_prevents_duplicate() {
        let plan = SendMessagePlan;
        let mut state = plan.initial_plan_state();
        state.phase = SendMessagePhase::Inputting;
        state.send_action_executed = true;

        let app_state = AppState::default();
        let identified = IdentifiedStates {
            main_window: None,
            popup: None,
            contact_card: None,
            settings: None,
        };
        let dummy_a11y = A11yNode {
            role: "application".to_string(),
            name: "WeChat".to_string(),
            states: None,
            bounds: None,
            parent_index: None,
            children: None,
            window: None,
        };
        let params = SendMessageParams {
            chat_id: "filehelper".to_string(),
            message: Some("hello".to_string()),
            image_path: None,
            image_mime: None,
            file_path: None,
        };

        let action = plan
            .select_action(
                &app_state,
                &params,
                &identified,
                &mut state,
                &dummy_a11y,
                "session_1",
            )
            .await;

        assert!(action.is_none(), "Guard must suppress duplicate action");
        assert!(
            matches!(state.phase, SendMessagePhase::Done),
            "State should transition to Done"
        );
    }

    #[test]
    fn test_filehelper_target_alias() {
        use crate::plans::multi_frame::is_target_chat_name_match;
        assert!(is_target_chat_name_match("filehelper", "File Transfer"));
        assert!(is_target_chat_name_match("filehelper", "文件传输助手"));
        assert!(is_target_chat_name_match("filehelper", "filehelper"));
        assert!(is_target_chat_name_match("Bob", "Bob"));
        assert!(!is_target_chat_name_match("Bob", "Alice"));
    }

    #[test]
    fn test_primary_open_error_classification_is_sanitized() {
        assert_eq!(
            classify_primary_open_error(
                "No sessions found. Is WeChat logged in with chats visible?"
            ),
            "session_enumeration_failed"
        );
        assert_eq!(
            classify_primary_open_error(
                "Failed to attach: bootstrapper failed due to auxv-not-found"
            ),
            "frida_attach_failed"
        );
        assert_eq!(
            classify_primary_open_error("target Bob was not found"),
            "primary_open_failed"
        );
    }

    #[tokio::test]
    async fn test_send_safe_fallback_search_never_emits_return() {
        let plan = SendMessagePlan;
        let mut plan_state = plan.initial_plan_state();
        plan_state.phase = SendMessagePhase::FallbackFocusingSearch;
        plan_state.resolved_target_name = Some("testB".to_string());

        let mut a11y = fixture("f4");
        let app = a11y.children.as_mut().unwrap().first_mut().unwrap();
        let main = app.children.as_mut().unwrap().first_mut().unwrap();
        main.children.as_mut().unwrap().push(A11yNode {
            role: "text".to_string(),
            name: "Search".to_string(),
            bounds: Some(Bounds {
                x: 80.0,
                y: 20.0,
                width: 170.0,
                height: 32.0,
            }),
            children: None,
            parent_index: None,
            window: None,
            states: Some(vec!["EDITABLE".to_string(), "FOCUSED".to_string()]),
        });

        let params = SendMessageParams {
            chat_id: "wxid_test".to_string(),
            message: Some("payload".to_string()),
            image_path: None,
            image_mime: None,
            file_path: None,
        };
        let action = plan
            .select_action(
                &AppState::default(),
                &params,
                &IdentifiedStates {
                    main_window: None,
                    popup: None,
                    contact_card: None,
                    settings: None,
                },
                &mut plan_state,
                &a11y,
                "test",
            )
            .await
            .expect("fallback search action must be generated");

        assert!(
            !action_contains_return(&action.action),
            "send-safe open fallback must never press Return/Enter"
        );
        assert!(matches!(
            plan_state.phase,
            SendMessagePhase::FallbackSelectingTarget
        ));
    }

    #[tokio::test]
    async fn test_fallback_exact_target_selection_is_click_only() {
        let plan = SendMessagePlan;
        let mut plan_state = plan.initial_plan_state();
        plan_state.phase = SendMessagePhase::FallbackSelectingTarget;
        plan_state.resolved_target_name = Some("testB".to_string());
        let a11y = fixture("f4");
        let params = SendMessageParams {
            chat_id: "wxid_test".to_string(),
            message: Some("payload".to_string()),
            image_path: None,
            image_mime: None,
            file_path: None,
        };

        let action = plan
            .select_action(
                &AppState::default(),
                &params,
                &IdentifiedStates {
                    main_window: None,
                    popup: None,
                    contact_card: None,
                    settings: None,
                },
                &mut plan_state,
                &a11y,
                "test",
            )
            .await
            .expect("exact target row must yield a click action");

        assert!(!action_contains_return(&action.action));
        assert!(matches!(
            plan_state.phase,
            SendMessagePhase::FallbackVerifyingTarget
        ));
    }

    #[tokio::test]
    async fn test_confirming_target_mismatch_fails_before_success() {
        let plan = SendMessagePlan;
        let mut plan_state = plan.initial_plan_state();
        plan_state.phase = SendMessagePhase::Confirming;
        plan_state.resolved_target_name = Some("Bob".to_string());
        plan_state.send_action_executed = true;
        let a11y = fixture("f6"); // active visible target is Alice
        let params = SendMessageParams {
            chat_id: "wxid_bob".to_string(),
            message: Some("payload".to_string()),
            image_path: None,
            image_mime: None,
            file_path: None,
        };

        let action = plan
            .select_action(
                &AppState::default(),
                &params,
                &IdentifiedStates {
                    main_window: None,
                    popup: None,
                    contact_card: None,
                    settings: None,
                },
                &mut plan_state,
                &a11y,
                "test",
            )
            .await;

        assert!(action.is_none());
        assert_eq!(
            plan_state.failure_reason.as_deref(),
            Some("confirm_target_not_verified:target_not_verified")
        );
        assert!(matches!(plan_state.phase, SendMessagePhase::Confirming));
    }

    #[test]
    fn test_diagnostic_display_does_not_expose_contact_names() {
        let err = SendPlannerError::TargetNotVerified {
            expected: "Sensitive Expected".to_string(),
            actual: Some("Sensitive Actual".to_string()),
        };
        assert_eq!(err.to_string(), "target_not_verified");

        let err = SendPlannerError::SearchAmbiguous {
            query: "Sensitive Query".to_string(),
            candidates: vec!["Alice".to_string(), "Alicia".to_string()],
        };
        assert_eq!(err.to_string(), "search_ambiguous:found_2_candidates");
    }
}
