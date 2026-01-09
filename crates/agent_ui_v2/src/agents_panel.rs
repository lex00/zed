use acp_thread::{AgentConnection, AgentSessionList, EmptySessionList};
use agent::NativeAgentConnection;
use agent_client_protocol as acp;
use agent_servers::{AgentServer, AgentServerDelegate};
use agent_settings::AgentSettings;
use agent_ui::{AgentKey, AgentSessionsModel};
use anyhow::Result;
use db::kvp::KEY_VALUE_STORE;
use feature_flags::{AgentV2FeatureFlag, FeatureFlagAppExt};
use fs::Fs;
use gpui::{
    Action, AsyncWindowContext, Entity, EventEmitter, Focusable, Pixels, Subscription, Task,
    WeakEntity, actions, prelude::*,
};
use project::Project;
use prompt_store::PromptStore;
use serde::{Deserialize, Serialize};
use settings::{Settings as _, update_settings_file};
use std::rc::Rc;
use std::sync::Arc;
use ui::{App, Context, IconName, IntoElement, ParentElement, Render, Styled, Window};
use util::ResultExt;
use workspace::{
    Panel, Workspace,
    dock::{ClosePane, DockPosition, PanelEvent, UtilityPane},
    utility_pane::{UtilityPaneSlot, utility_slot_for_dock_position},
};

use crate::agent_thread_pane::{
    AgentThreadPane, AgentsUtilityPaneEvent, SerializedAgentThreadPane,
};
use crate::thread_history::{AcpThreadHistory, ThreadHistoryEvent};

const AGENTS_PANEL_KEY: &str = "agents_panel";

#[derive(Serialize, Deserialize, Debug)]
struct SerializedAgentsPanel {
    width: Option<Pixels>,
    pane: Option<SerializedAgentThreadPane>,
}

actions!(
    agents,
    [
        /// Toggle the visibility of the agents panel.
        ToggleAgentsPanel
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleAgentsPanel, window, cx| {
            workspace.toggle_panel_focus::<AgentsPanel>(window, cx);
        });
    })
    .detach();
}

pub struct AgentsPanel {
    focus_handle: gpui::FocusHandle,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    agent_thread_pane: Option<Entity<AgentThreadPane>>,
    history: Entity<AcpThreadHistory>,
    agent_session_list: Option<Rc<dyn AgentSessionList>>,
    agent_sessions_model: Entity<AgentSessionsModel>,
    prompt_store: Option<Entity<PromptStore>>,
    fs: Arc<dyn Fs>,
    width: Option<Pixels>,
    pending_serialization: Task<Option<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AgentsPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<Result<Entity<Self>, anyhow::Error>> {
        cx.spawn(async move |cx| {
            let serialized_panel = cx
                .background_spawn(async move {
                    KEY_VALUE_STORE
                        .read_kvp(AGENTS_PANEL_KEY)
                        .ok()
                        .flatten()
                        .and_then(|panel| {
                            serde_json::from_str::<SerializedAgentsPanel>(&panel).ok()
                        })
                })
                .await;

            let (fs, project) = workspace.update(cx, |workspace, _cx| {
                let fs = workspace.app_state().fs.clone();
                let project = workspace.project().clone();
                (fs, project)
            })?;

            let prompt_store = workspace
                .update(cx, |_, cx| PromptStore::global(cx))?
                .await
                .log_err();

            workspace.update_in(cx, |_, window, cx| {
                cx.new(|cx| {
                    let mut panel =
                        Self::new(workspace.clone(), fs, project, prompt_store, window, cx);
                    if let Some(serialized_panel) = serialized_panel {
                        panel.width = serialized_panel.width;
                        if let Some(serialized_pane) = serialized_panel.pane {
                            panel.restore_utility_pane(serialized_pane, window, cx);
                        }
                    }
                    panel
                })
            })
        })
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        fs: Arc<dyn Fs>,
        project: Entity<Project>,
        prompt_store: Option<Entity<PromptStore>>,
        window: &mut Window,
        cx: &mut ui::Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        // Initialize with empty session list - will be populated when we have an agent connection
        let session_list: Rc<dyn AgentSessionList> = Rc::new(EmptySessionList);
        let history = cx.new(|cx| AcpThreadHistory::new(session_list.clone(), window, cx));

        let this = cx.weak_entity();
        let subscriptions = vec![
            cx.subscribe_in(&history, window, Self::handle_history_event),
            cx.on_flags_ready(move |_, cx| {
                this.update(cx, |_, cx| {
                    cx.notify();
                })
                .ok();
            }),
        ];

        let mut panel = Self {
            focus_handle,
            workspace,
            project,
            agent_thread_pane: None,
            history,
            agent_session_list: None,
            prompt_store,
            agent_sessions_model: cx.new(|cx| AgentSessionsModel::new(cx)),
            fs,
            width: None,
            pending_serialization: Task::ready(None),
            _subscriptions: subscriptions,
        };

        // Initialize the session list from native agent
        panel.initialize_session_list(cx);

        panel
    }

    fn initialize_session_list(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        let project = self.project.clone();

        // Create a native agent connection to get the session list
        let delegate = AgentServerDelegate::new(
            project.read(cx).agent_server_store().clone(),
            project.clone(),
            None,
            None,
        );

        let server = Rc::new(agent::NativeAgentServer::new(fs));
        let connection_task = server.connect(None, delegate, cx);

        cx.spawn(async move |this, cx| {
            let (connection, _): (Rc<dyn AgentConnection>, _) = match connection_task.await {
                Ok(result) => result,
                Err(err) => {
                    log::error!("Failed to create agent connection for session list: {err:?}");
                    return;
                }
            };

            let native_connection: Rc<NativeAgentConnection> =
                match connection.downcast::<NativeAgentConnection>() {
                    Some(conn) => conn,
                    None => {
                        log::error!("Failed to downcast to NativeAgentConnection");
                        return;
                    }
                };

            this.update(cx, |this, cx| {
                if let Some(session_list) = native_connection.session_list(cx) {
                    this.agent_session_list = Some(session_list.clone());
                    this.history.update(cx, |history, cx| {
                        history.set_session_list(session_list.clone(), cx);
                    });

                    // Keep the new model in sync so the pane can derive metadata for resume/open.
                    this.agent_sessions_model.update(cx, |model, cx| {
                        // v2 currently uses native agent sessions.
                        model.set_agent(AgentKey::Native, Some(session_list), cx);
                        model.refresh(cx);
                    });
                } else {
                    this.agent_sessions_model.update(cx, |model, cx| {
                        model.clear(cx);
                    });
                }
            })
            .log_err();
        })
        .detach();
    }

    fn restore_utility_pane(
        &mut self,
        serialized_pane: SerializedAgentThreadPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session_id_str) = &serialized_pane.session_id else {
            return;
        };

        let session_id = acp::SessionId::new(session_id_str.clone());
        self.open_session(
            session_id,
            serialized_pane.expanded,
            serialized_pane.width,
            window,
            cx,
        );
    }

    fn handle_utility_pane_event(
        &mut self,
        _utility_pane: Entity<AgentThreadPane>,
        event: &AgentsUtilityPaneEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            AgentsUtilityPaneEvent::StateChanged => {
                self.serialize(cx);
                cx.notify();
            }
        }
    }

    fn handle_close_pane_event(
        &mut self,
        _utility_pane: Entity<AgentThreadPane>,
        _event: &ClosePane,
        cx: &mut Context<Self>,
    ) {
        self.agent_thread_pane = None;
        self.serialize(cx);
        cx.notify();
    }

    fn handle_history_event(
        &mut self,
        _history: &Entity<AcpThreadHistory>,
        event: &ThreadHistoryEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ThreadHistoryEvent::OpenSession(session_id) => {
                self.open_session(session_id.clone(), true, None, window, cx);
            }
        }
    }

    fn open_session(
        &mut self,
        session_id: acp::SessionId,
        expanded: bool,
        width: Option<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Check if we already have this session open
        if let Some(existing_pane) = &self.agent_thread_pane {
            if existing_pane.read(cx).session_id() == Some(&session_id) {
                existing_pane.update(cx, |pane, cx| {
                    pane.set_expanded(true, cx);
                });
                return;
            }
        }

        let fs = self.fs.clone();
        let workspace = self.workspace.clone();
        let project = self.project.clone();
        let prompt_store = self.prompt_store.clone();
        let agent_sessions_model = self.agent_sessions_model.clone();

        let agent_thread_pane = cx.new(|cx| {
            let mut pane = AgentThreadPane::new(workspace.clone(), cx);
            pane.open_session(
                session_id,
                fs,
                workspace.clone(),
                project,
                prompt_store,
                agent_sessions_model,
                window,
                cx,
            );
            if let Some(width) = width {
                pane.set_width(Some(width), cx);
            }
            pane.set_expanded(expanded, cx);
            pane
        });

        let state_subscription = cx.subscribe(&agent_thread_pane, Self::handle_utility_pane_event);
        let close_subscription = cx.subscribe(&agent_thread_pane, Self::handle_close_pane_event);

        self._subscriptions.push(state_subscription);
        self._subscriptions.push(close_subscription);

        let slot = self.utility_slot(window, cx);
        let panel_id = cx.entity_id();

        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.register_utility_pane(slot, panel_id, agent_thread_pane.clone(), cx);
            });
        }

        self.agent_thread_pane = Some(agent_thread_pane);
        self.serialize(cx);
        cx.notify();
    }

    fn utility_slot(&self, window: &Window, cx: &App) -> UtilityPaneSlot {
        let position = self.position(window, cx);
        utility_slot_for_dock_position(position)
    }

    fn re_register_utility_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = &self.agent_thread_pane {
            let slot = self.utility_slot(window, cx);
            let panel_id = cx.entity_id();
            let pane = pane.clone();

            if let Some(workspace) = self.workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    workspace.register_utility_pane(slot, panel_id, pane, cx);
                });
            }
        }
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        let width = self.width;
        let pane = self
            .agent_thread_pane
            .as_ref()
            .map(|pane| pane.read(cx).serialize());

        self.pending_serialization = cx.background_spawn(async move {
            KEY_VALUE_STORE
                .write_kvp(
                    AGENTS_PANEL_KEY.into(),
                    serde_json::to_string(&SerializedAgentsPanel { width, pane }).unwrap(),
                )
                .await
                .log_err()
        });
    }
}

impl EventEmitter<PanelEvent> for AgentsPanel {}

impl Focusable for AgentsPanel {
    fn focus_handle(&self, _cx: &ui::App) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for AgentsPanel {
    fn persistent_name() -> &'static str {
        "AgentsPanel"
    }

    fn panel_key() -> &'static str {
        AGENTS_PANEL_KEY
    }

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        match AgentSettings::get_global(cx).agents_panel_dock {
            settings::DockSide::Left => DockPosition::Left,
            settings::DockSide::Right => DockPosition::Right,
        }
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position != DockPosition::Bottom
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings.agent.get_or_insert_default().agents_panel_dock = Some(match position {
                DockPosition::Left => settings::DockSide::Left,
                DockPosition::Right | DockPosition::Bottom => settings::DockSide::Right,
            });
        });
        self.re_register_utility_pane(window, cx);
    }

    fn size(&self, window: &Window, cx: &App) -> Pixels {
        let settings = AgentSettings::get_global(cx);
        match self.position(window, cx) {
            DockPosition::Left | DockPosition::Right => {
                self.width.unwrap_or(settings.default_width)
            }
            DockPosition::Bottom => self.width.unwrap_or(settings.default_height),
        }
    }

    fn set_size(&mut self, size: Option<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        match self.position(window, cx) {
            DockPosition::Left | DockPosition::Right => self.width = size,
            DockPosition::Bottom => {}
        }
        self.serialize(cx);
        cx.notify();
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<IconName> {
        (self.enabled(cx) && AgentSettings::get_global(cx).button).then_some(IconName::ZedAgentTwo)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Agents Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleAgentsPanel)
    }

    fn activation_priority(&self) -> u32 {
        4
    }

    fn enabled(&self, cx: &App) -> bool {
        AgentSettings::get_global(cx).enabled(cx) && cx.has_flag::<AgentV2FeatureFlag>()
    }
}

impl Render for AgentsPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::div().size_full().child(self.history.clone())
    }
}
