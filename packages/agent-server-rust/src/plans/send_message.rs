use super::Plan;
use crate::ia::actions;
use crate::ia::selectors::query_selector;
use crate::ia::types::*;
use crate::plans::multi_frame::{node_has_state, resolve_active_composer, verify_target_chat};
use crate::tools::chat_select::{open_chat, OpenChatResult};
use crate::tools::exec::{exec_command, ExecOptions};

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

                    // Port Item 4 & Fixture F7: Verify if desired target is already open.
                    // If positively verified, skip redundant open_chat and proceed to focusing.
                    match verify_target_chat(a11y, &params.chat_id) {
                        Ok(true) => {
                            tracing::info!(
                                "[send] target '{}' already verified open, proceeding directly to focusing",
                                params.chat_id
                            );
                            plan_state.phase = SendMessagePhase::Focusing;
                            continue;
                        }
                        Err(err) => {
                            tracing::info!(
                                "[send] active chat mismatch ({}), initiating open_chat for '{}'",
                                err,
                                params.chat_id
                            );
                        }
                        Ok(false) => {
                            tracing::info!(
                                "[send] no active chat detected, opening target '{}'",
                                params.chat_id
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
                        tracing::error!(
                            "[send] open_chat failed for target '{}': {}",
                            params.chat_id,
                            err_str
                        );
                        plan_state.failure_reason = Some(format!("open_chat_failed:{}", err_str));
                        plan_state.open_result = Some(result);
                        return None;
                    }

                    let skipped = result.skipped.unwrap_or(false);
                    plan_state.open_result = Some(result);
                    plan_state.phase = SendMessagePhase::Focusing;

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

                    // Port Item 4 & Fixture F6: Positive target verification before typing
                    match verify_target_chat(a11y, &params.chat_id) {
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
                            tracing::warn!(
                                "[send] target chat header not detected for '{}'",
                                params.chat_id
                            );
                            plan_state.failure_reason = Some(format!(
                                "target_not_verified:expected='{}',actual='none'",
                                params.chat_id
                            ));
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
}
