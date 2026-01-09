use agent::{DbThreadMetadata, NativeAgentServer};
use agent_client_protocol as acp;
use agent_servers::AgentServer;
use agent_settings::AgentSettings;
use agent_ui::AgentSessionsModel;
use agent_ui::acp::AcpThreadView;
use chrono::Utc;
use fs::Fs;
use gpui::{
    Entity, EventEmitter, Focusable, Pixels, SharedString, Subscription, WeakEntity, prelude::*,
};
use project::Project;
use prompt_store::PromptStore;
use serde::{Deserialize, Serialize};
use settings::DockSide;
use settings::Settings as _;
use std::rc::Rc;
use std::sync::Arc;
use ui::{Tab, Tooltip, prelude::*};
use workspace::{
    Workspace,
    dock::{ClosePane, MinimizePane, UtilityPane, UtilityPanePosition},
    utility_pane::UtilityPaneSlot,
};

pub const DEFAULT_UTILITY_PANE_WIDTH: Pixels = gpui::px(400.0);

#[derive(Serialize, Deserialize, Debug)]
pub struct SerializedAgentThreadPane {
    pub expanded: bool,
    pub width: Option<Pixels>,
    pub session_id: Option<String>,
}

pub enum AgentsUtilityPaneEvent {
    StateChanged,
}

impl EventEmitter<AgentsUtilityPaneEvent> for AgentThreadPane {}
impl EventEmitter<MinimizePane> for AgentThreadPane {}
impl EventEmitter<ClosePane> for AgentThreadPane {}

struct ActiveThreadView {
    view: Entity<AcpThreadView>,
    session_id: acp::SessionId,
    _notify: Subscription,
}

pub struct AgentThreadPane {
    focus_handle: gpui::FocusHandle,
    expanded: bool,
    width: Option<Pixels>,
    thread_view: Option<ActiveThreadView>,
    workspace: WeakEntity<Workspace>,
}

impl AgentThreadPane {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut ui::Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        Self {
            focus_handle,
            expanded: false,
            width: None,
            thread_view: None,
            workspace,
        }
    }

    pub fn session_id(&self) -> Option<&acp::SessionId> {
        self.thread_view.as_ref().map(|tv| &tv.session_id)
    }

    pub fn serialize(&self) -> SerializedAgentThreadPane {
        SerializedAgentThreadPane {
            expanded: self.expanded,
            width: self.width,
            session_id: self.session_id().map(|id| id.to_string()),
        }
    }

    pub fn open_session(
        &mut self,
        session_id: acp::SessionId,
        fs: Arc<dyn Fs>,
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        prompt_store: Option<Entity<PromptStore>>,
        agent_sessions_model: Entity<AgentSessionsModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let resume_thread = if let Some(session) = agent_sessions_model
            .read(cx)
            .sessions
            .iter()
            .find(|session| session.session_id == session_id)
        {
            DbThreadMetadata {
                title: session.display_title(),
                id: session.session_id.clone(),
                updated_at: session.updated_at.unwrap_or_else(Utc::now),
            }
        } else {
            DbThreadMetadata {
                id: session_id.clone(),
                title: "New Thread".into(),
                updated_at: Utc::now(),
            }
        };

        let agent: Rc<dyn AgentServer> = Rc::new(NativeAgentServer::new(fs));

        let thread_view = cx.new(|cx| {
            AcpThreadView::new(
                agent,
                Some(resume_thread),
                None,
                workspace,
                project,
                prompt_store,
                agent_sessions_model,
                true,
                window,
                cx,
            )
        });

        let notify = cx.observe(&thread_view, |_, _, cx| {
            cx.notify();
        });

        self.thread_view = Some(ActiveThreadView {
            view: thread_view,
            session_id,
            _notify: notify,
        });

        cx.notify();
    }

    fn title(&self, cx: &App) -> SharedString {
        if let Some(active_thread_view) = &self.thread_view {
            let thread_view = active_thread_view.view.read(cx);
            if let Some(thread) = thread_view.thread() {
                let title = thread.read(cx).title();
                if !title.is_empty() {
                    return title;
                }
            }
            thread_view.title(cx)
        } else {
            "Thread".into()
        }
    }

    fn render_header(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let position = self.position(window, cx);
        let slot = match position {
            UtilityPanePosition::Left => UtilityPaneSlot::Left,
            UtilityPanePosition::Right => UtilityPaneSlot::Right,
        };

        let workspace = self.workspace.clone();
        let toggle_icon = self.toggle_icon(cx);
        let title = self.title(cx);

        let pane_toggle_button = |workspace: WeakEntity<Workspace>| {
            IconButton::new("toggle_utility_pane", toggle_icon)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text("Toggle Agent Pane"))
                .on_click(move |_, window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.toggle_utility_pane(slot, window, cx)
                        })
                        .ok();
                })
        };

        h_flex()
            .id("utility-pane-header")
            .w_full()
            .h(Tab::container_height(cx))
            .px_1p5()
            .gap(DynamicSpacing::Base06.rems(cx))
            .when(slot == UtilityPaneSlot::Right, |this| {
                this.flex_row_reverse()
            })
            .flex_none()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(pane_toggle_button(workspace))
            .child(
                h_flex()
                    .size_full()
                    .min_w_0()
                    .gap_1()
                    .map(|this| {
                        if slot == UtilityPaneSlot::Right {
                            this.flex_row_reverse().justify_start()
                        } else {
                            this.justify_between()
                        }
                    })
                    .child(Label::new(title).truncate())
                    .child(
                        IconButton::new("close_btn", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Close Agent Pane"))
                            .on_click(cx.listener(|this, _: &gpui::ClickEvent, _window, cx| {
                                cx.emit(ClosePane);
                                this.thread_view = None;
                                cx.notify()
                            })),
                    ),
            )
    }
}

impl Focusable for AgentThreadPane {
    fn focus_handle(&self, cx: &ui::App) -> gpui::FocusHandle {
        if let Some(thread_view) = &self.thread_view {
            thread_view.view.focus_handle(cx)
        } else {
            self.focus_handle.clone()
        }
    }
}

impl UtilityPane for AgentThreadPane {
    fn position(&self, _window: &Window, cx: &App) -> UtilityPanePosition {
        match AgentSettings::get_global(cx).agents_panel_dock {
            DockSide::Left => UtilityPanePosition::Left,
            DockSide::Right => UtilityPanePosition::Right,
        }
    }

    fn toggle_icon(&self, _cx: &App) -> IconName {
        IconName::Thread
    }

    fn expanded(&self, _cx: &App) -> bool {
        self.expanded
    }

    fn set_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        self.expanded = expanded;
        cx.emit(AgentsUtilityPaneEvent::StateChanged);
        cx.notify();
    }

    fn width(&self, _cx: &App) -> Pixels {
        self.width.unwrap_or(DEFAULT_UTILITY_PANE_WIDTH)
    }

    fn set_width(&mut self, width: Option<Pixels>, cx: &mut Context<Self>) {
        self.width = width;
        cx.emit(AgentsUtilityPaneEvent::StateChanged);
        cx.notify();
    }
}

impl Render for AgentThreadPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = if let Some(thread_view) = &self.thread_view {
            div().size_full().child(thread_view.view.clone())
        } else {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(Label::new("Select a thread to view details").size(LabelSize::Default))
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_header(window, cx))
            .child(content)
    }
}
