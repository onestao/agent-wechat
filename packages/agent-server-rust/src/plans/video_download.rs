//! Make WeChat download a received video or original image.
//!
//! The Linux client stores only the cover (`_thumb.jpg`) of a video until the
//! video bubble is clicked, and only the chat-size image (`.dat`) until the
//! image is opened in the viewer (which downloads the original `_h.dat`).
//! This plan opens the chat, clicks the matching bubble, waits for the
//! download, and closes the viewer/player with Escape.

use super::Plan;
use crate::ia::actions;
use crate::ia::selectors::query_selector_all;
use crate::ia::types::*;
use crate::tools::chat_select::open_chat;

pub struct VideoDownloadPlan;

/// Which kind of bubble to click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BubbleKind {
    /// Video; its length in seconds (`playlength`) picks the right bubble.
    Video { duration_secs: Option<u32> },
    /// Image; the most recent image bubble is clicked.
    Image,
}

pub struct VideoDownloadParams {
    pub chat_id: String,
    pub kind: BubbleKind,
    /// Sent by the logged-in account: bubble is on the right, otherwise left.
    pub is_self: bool,
}

pub enum VideoDownloadPhase {
    Opening,
    Locating,
    Done,
}

pub struct VideoDownloadPlanState {
    pub phase: VideoDownloadPhase,
    pub error: Option<String>,
    locate_attempts: u8,
}

/// Horizontal offset from the message row edge to the middle of a video
/// bubble (avatar + margin + half the thumbnail), verified on WeChat 4.1.
const BUBBLE_EDGE_OFFSET: f64 = 150.0;
/// Time to let WeChat download the video / original image after the click.
const VIDEO_DOWNLOAD_WAIT_MS: u64 = 6000;
const IMAGE_DOWNLOAD_WAIT_MS: u64 = 3000;
const MAX_LOCATE_ATTEMPTS: u8 = 3;

/// Parse a "Video0:04" / "视频 1:05" list-item label into seconds.
fn label_duration_secs(label: &str) -> Option<u32> {
    let digits: String = label
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == ':')
        .collect();
    let mut parts = digits.split(':').rev();
    let secs: u32 = parts.next()?.parse().ok()?;
    let mins: u32 = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    let hours: u32 = parts.next().and_then(|h| h.parse().ok()).unwrap_or(0);
    Some(hours * 3600 + mins * 60 + secs)
}

/// Find where to click the most recent bubble of `kind` (for videos, the most
/// recent one matching the duration, else the most recent video).
pub(crate) fn find_bubble_click(
    a11y: &A11yNode,
    kind: BubbleKind,
    is_self: bool,
) -> Option<(f64, f64)> {
    let (selector, duration_secs) = match kind {
        BubbleKind::Video { duration_secs } => (
            r#"list[name="Messages"] > list-item[name=/^\s*(Video|视频)/]"#,
            duration_secs,
        ),
        BubbleKind::Image => (
            r#"list[name="Messages"] > list-item[name=/^\s*(Image|Photo|图片)/]"#,
            None,
        ),
    };
    let items = query_selector_all(a11y, selector);
    let matching: Vec<&&A11yNode> = match duration_secs {
        Some(d) => items
            .iter()
            .filter(|n| label_duration_secs(&n.name) == Some(d))
            .collect(),
        None => Vec::new(),
    };
    let target = matching.last().copied().or_else(|| items.last())?;
    let b = target.bounds.as_ref()?;
    let x = if is_self {
        b.x + b.width - BUBBLE_EDGE_OFFSET
    } else {
        b.x + BUBBLE_EDGE_OFFSET
    };
    Some((x.round(), (b.y + b.height / 2.0).round()))
}

#[async_trait::async_trait]
impl Plan for VideoDownloadPlan {
    type PlanState = VideoDownloadPlanState;
    type Params = VideoDownloadParams;

    fn id(&self) -> &str {
        "video_download"
    }

    fn initial_plan_state(&self) -> VideoDownloadPlanState {
        VideoDownloadPlanState {
            phase: VideoDownloadPhase::Opening,
            error: None,
            locate_attempts: 0,
        }
    }

    fn is_goal_reached(&self, _state: &AppState, plan_state: &VideoDownloadPlanState) -> bool {
        matches!(plan_state.phase, VideoDownloadPhase::Done) && plan_state.error.is_none()
    }

    fn failure_reason(&self, plan_state: &VideoDownloadPlanState) -> Option<String> {
        plan_state.error.clone()
    }

    async fn select_action(
        &self,
        state: &AppState,
        params: &VideoDownloadParams,
        identified: &IdentifiedStates,
        plan_state: &mut VideoDownloadPlanState,
        a11y: &A11yNode,
        _session_id: &str,
    ) -> Option<SelectedAction> {
        let frame = identified
            .main_window
            .as_ref()
            .and_then(|m| m.frame.clone());

        if state.popup.is_some() && identified.popup.is_some() {
            return Some(SelectedAction {
                action: actions::dismiss_popup(),
                frame,
            });
        }

        let main_state_id = identified.main_window.as_ref().map(|m| m.state_id.as_str());

        match plan_state.phase {
            VideoDownloadPhase::Opening => {
                if main_state_id != Some("chat") && main_state_id != Some("chat_open") {
                    plan_state.error = Some(format!("unexpected main state {main_state_id:?}"));
                    return None;
                }
                let force = main_state_id == Some("chat");
                let result = open_chat(&params.chat_id, force, None).await;
                if !result.ok {
                    plan_state.error = Some("chat open failed".to_string());
                    return None;
                }
                plan_state.phase = VideoDownloadPhase::Locating;
                Some(SelectedAction {
                    action: actions::wait_short(),
                    frame,
                })
            }
            VideoDownloadPhase::Locating => {
                match find_bubble_click(a11y, params.kind, params.is_self) {
                    Some((x, y)) => {
                        let wait_ms = match params.kind {
                            BubbleKind::Video { .. } => VIDEO_DOWNLOAD_WAIT_MS,
                            BubbleKind::Image => IMAGE_DOWNLOAD_WAIT_MS,
                        };
                        tracing::info!(
                            "[video_download] clicking {:?} bubble at ({x}, {y})",
                            params.kind
                        );
                        plan_state.phase = VideoDownloadPhase::Done;
                        Some(SelectedAction {
                            action: actions::sequence(vec![
                                actions::click_at(x, y),
                                actions::wait(wait_ms),
                                Action::Key {
                                    combo: "Escape".to_string(),
                                },
                            ]),
                            frame,
                        })
                    }
                    None => {
                        plan_state.locate_attempts += 1;
                        if plan_state.locate_attempts >= MAX_LOCATE_ATTEMPTS {
                            plan_state.error = Some("media bubble not found".to_string());
                            plan_state.phase = VideoDownloadPhase::Done;
                            return None;
                        }
                        Some(SelectedAction {
                            action: actions::wait_short(),
                            frame,
                        })
                    }
                }
            }
            VideoDownloadPhase::Done => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(role: &str, name: &str, bounds: Option<Bounds>, children: Vec<A11yNode>) -> A11yNode {
        A11yNode {
            role: role.to_string(),
            name: name.to_string(),
            bounds,
            children: if children.is_empty() {
                None
            } else {
                Some(children)
            },
            parent_index: None,
            window: None,
            states: None,
        }
    }

    fn row(name: &str, y: f64) -> A11yNode {
        node(
            "list-item",
            name,
            Some(Bounds {
                x: 423.0,
                y,
                width: 704.0,
                height: 270.0,
            }),
            vec![],
        )
    }

    fn messages(rows: Vec<A11yNode>) -> A11yNode {
        let list = node("list", "Messages", None, rows);
        node("frame", "Weixin", None, vec![list])
    }

    #[test]
    fn test_label_duration_secs() {
        assert_eq!(label_duration_secs("Video0:04\n"), Some(4));
        assert_eq!(label_duration_secs("视频 1:05"), Some(65));
        assert_eq!(label_duration_secs("Video1:02:03"), Some(3723));
        assert_eq!(label_duration_secs("Video"), None);
    }

    #[test]
    fn test_find_bubble_click_video_matches_duration_and_side() {
        let tree = messages(vec![
            row("Video0:04\n", 0.0),
            row("Hello", 280.0),
            row("Video0:10\n", 323.0),
        ]);
        // Duration picks the 0:04 video even though it is not the last one.
        assert_eq!(
            find_bubble_click(
                &tree,
                BubbleKind::Video {
                    duration_secs: Some(4)
                },
                true
            ),
            Some((977.0, 135.0))
        );
        // Incoming video: bubble on the left.
        assert_eq!(
            find_bubble_click(
                &tree,
                BubbleKind::Video {
                    duration_secs: Some(10)
                },
                false
            ),
            Some((573.0, 458.0))
        );
        // Unknown duration: most recent video.
        assert_eq!(
            find_bubble_click(
                &tree,
                BubbleKind::Video {
                    duration_secs: Some(99)
                },
                true
            ),
            Some((977.0, 458.0))
        );
    }

    #[test]
    fn test_find_bubble_click_none_without_media() {
        let tree = messages(vec![row("Hello", 0.0)]);
        assert_eq!(
            find_bubble_click(
                &tree,
                BubbleKind::Video {
                    duration_secs: Some(4)
                },
                true
            ),
            None
        );
        assert_eq!(find_bubble_click(&tree, BubbleKind::Image, true), None);
    }

    #[test]
    fn test_find_bubble_click_image_picks_most_recent() {
        let tree = messages(vec![
            row("Image", 0.0),
            row("Video0:04", 280.0),
            row("Image", 560.0),
        ]);
        assert_eq!(
            find_bubble_click(&tree, BubbleKind::Image, false),
            Some((573.0, 695.0))
        );
    }
}
