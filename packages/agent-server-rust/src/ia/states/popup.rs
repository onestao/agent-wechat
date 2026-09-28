use crate::ia::selectors::query_selector;
use crate::ia::types::*;

/// Error popup.
struct PopupErrorState;

/// Check if Settings frame is present (popups inside Settings are handled by settings FSM).
fn has_settings_frame(a11y: &A11yNode) -> bool {
    query_selector(a11y, r#"frame[name="Settings"]"#).is_some()
}

impl IAState for PopupErrorState {
    fn fsm(&self) -> &str {
        "popup"
    }
    fn id(&self) -> &str {
        "popup_error"
    }

    fn identify(&self, args: &IdentifyArgs) -> Result<IdentifyResult, String> {
        // Exclude matches when Settings frame is open (settings_modal handles those)
        if has_settings_frame(args.a11y) {
            return Ok(IdentifyResult {
                identified: false,
                frame: None,
            });
        }

        let ok_btn = query_selector(args.a11y, r#"push-button[name="OK"]"#);
        let error_text = query_selector(
            args.a11y,
            r#"static[name=/error|failed|timeout|失败|错误/i]"#,
        )
        .or_else(|| {
            query_selector(
                args.a11y,
                r#"label[name=/error|failed|timeout|失败|错误/i]"#,
            )
        });

        Ok(IdentifyResult {
            identified: ok_btn.is_some() && error_text.is_some(),
            frame: None,
        })
    }

    fn reduce(&self, args: &ReduceArgs) -> AppState {
        let error_text = query_selector(
            args.a11y,
            r#"static[name=/error|failed|timeout|失败|错误/i]"#,
        )
        .or_else(|| {
            query_selector(
                args.a11y,
                r#"label[name=/error|failed|timeout|失败|错误/i]"#,
            )
        });

        let mut state = args.prev.clone();
        state.popup = Some(PopupState {
            popup_type: PopupType::Error,
            message: error_text.map(|n| n.name.clone()),
        });
        state
    }
}

/// Confirm/Tip popup.
struct PopupConfirmState;

impl IAState for PopupConfirmState {
    fn fsm(&self) -> &str {
        "popup"
    }
    fn id(&self) -> &str {
        "popup_confirm"
    }

    fn identify(&self, args: &IdentifyArgs) -> Result<IdentifyResult, String> {
        // Exclude matches when Settings frame is open (settings_modal handles those)
        if has_settings_frame(args.a11y) {
            return Ok(IdentifyResult {
                identified: false,
                frame: None,
            });
        }

        let ok_btn = query_selector(args.a11y, r#"push-button[name=/^\s*(OK|Confirm|确定|确认)\s*$/i]"#);
        if ok_btn.is_none() {
            return Ok(IdentifyResult {
                identified: false,
                frame: None,
            });
        }

        let error_in_static = query_selector(
            args.a11y,
            r#"static[name=/error|failed|timeout|失败|错误/i]"#,
        )
        .is_some();
        let error_in_label = query_selector(
            args.a11y,
            r#"label[name=/error|failed|timeout|失败|错误/i]"#,
        )
        .is_some();
        if error_in_static || error_in_label {
            return Ok(IdentifyResult {
                identified: false,
                frame: None,
            });
        }

        Ok(IdentifyResult {
            identified: true,
            frame: None,
        })
    }

    fn reduce(&self, args: &ReduceArgs) -> AppState {
        let message_el = query_selector(args.a11y, r#"static[name=/.+/]"#)
            .or_else(|| query_selector(args.a11y, r#"label[name=/^(?!Tip$).+/]"#));

        let mut state = args.prev.clone();
        state.popup = Some(PopupState {
            popup_type: PopupType::Confirm,
            message: message_el.map(|n| n.name.clone()),
        });
        state
    }
}

pub static POPUP_STATES: std::sync::LazyLock<Vec<Box<dyn IAState>>> =
    std::sync::LazyLock::new(|| vec![Box::new(PopupErrorState), Box::new(PopupConfirmState)]);

#[cfg(test)]
mod tests {
    use crate::ia::identify_states;
    use crate::ia::types::A11yNode;

    fn chat_view_with_button(name: &str) -> A11yNode {
        let mut a11y: A11yNode =
            serde_json::from_str(include_str!("test_fixtures/chat_view.json")).unwrap();
        let button = A11yNode {
            role: "push-button".to_string(),
            name: name.to_string(),
            states: None,
            bounds: None,
            parent_index: None,
            children: None,
            window: None,
        };
        a11y.children.get_or_insert_with(Vec::new).push(button);
        a11y
    }

    #[test]
    fn test_chat_preview_containing_ok_is_not_a_popup() {
        // A chat-list item whose preview text contains "ok" (e.g. "mokoko")
        // used to be taken for a confirm dialog, and the dismiss action kept
        // clicking that chat forever.
        for name in ["泡泡玛特出求群 180求寻找mokoko×10", "Booking", "请确认收货地址"] {
            let states = identify_states(&chat_view_with_button(name), "");
            assert!(states.popup.is_none(), "false popup for {name:?}");
        }
    }

    #[test]
    fn test_real_confirm_buttons_are_popups() {
        for name in ["OK", "Confirm", "确定", " 确认 "] {
            let states = identify_states(&chat_view_with_button(name), "");
            assert_eq!(
                states.popup.as_ref().map(|p| p.state_id.as_str()),
                Some("popup_confirm"),
                "missed popup for {name:?}"
            );
        }
    }
}
