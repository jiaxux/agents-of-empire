//! Fleet operations view over immutable snapshots.
//!
//! One cohesive panel over the seven fleet capabilities rather than seven
//! modal systems. It renders whatever [`FleetSnapshot`] the state layer last
//! published; it never scans a repository, and it never mutates a session.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;

use crate::fleet::{
    AttentionReason, CheckpointState, EvidenceKind, EvidenceState, ReadinessVerdict,
};
use crate::trials::{ContenderState, TrialState};
use crate::tui::fleet_state::{
    CampaignLoadState, CheckpointLoadState, FleetSnapshot, ScanState, TrialLoadState,
};
use crate::tui::styles::Theme;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FleetTab {
    Attention,
    Conflicts,
    Readiness,
    Campaign,
    Budget,
    Trials,
    Checkpoints,
}

impl FleetTab {
    pub const ALL: [FleetTab; 7] = [
        Self::Attention,
        Self::Conflicts,
        Self::Readiness,
        Self::Campaign,
        Self::Budget,
        Self::Trials,
        Self::Checkpoints,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Attention => "Attention",
            Self::Conflicts => "Conflicts",
            Self::Readiness => "Readiness",
            Self::Campaign => "Campaign",
            Self::Budget => "Budget",
            Self::Trials => "Trials",
            Self::Checkpoints => "Checkpoints",
        }
    }
}

pub enum FleetDialogResult {
    Continue,
    Cancel,
    /// Select the session that owns the highlighted row and close.
    Jump(String),
    LoadCheckpoints,
    LoadTrials,
    LoadCampaigns,
    RefreshConflicts,
    AcknowledgeAttention {
        item_id: String,
    },
    CoordinateConflict {
        session_id: String,
    },
    CaptureCheckpoint {
        session_id: String,
        label: String,
    },
    ForkCheckpoint {
        checkpoint_id: String,
        session_id: String,
    },
    InspectCheckpoint {
        checkpoint_id: String,
    },
    ExportCheckpoint {
        checkpoint_id: String,
    },
    DeleteCheckpoint {
        checkpoint_id: String,
    },
    RestoreCheckpoint {
        checkpoint_id: String,
        session_id: String,
    },
    CreateCampaignTask {
        campaign_id: Option<String>,
        title: String,
        dependency_ids: Vec<String>,
        session_id: Option<String>,
    },
    UpdateCampaignTask {
        campaign_id: String,
        task_id: String,
        state: crate::campaigns::CampaignTaskState,
    },
    RetryTrialContender {
        trial_id: String,
        contender_id: String,
    },
    CancelTrial {
        trial_id: String,
    },
    RetainTrialWinner {
        trial_id: String,
        contender_id: String,
    },
    CreateTrial {
        checkpoint_id: String,
        session_id: String,
        prompt: String,
        verification_command: String,
        tools: Vec<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CheckpointTarget {
    pub(crate) session_id: String,
    pub(crate) title: String,
}

#[derive(Clone, Debug)]
struct TrialPromptEditor {
    checkpoint_id: String,
    session_id: String,
    prompt: String,
}

#[derive(Clone, Debug)]
struct TrialToolChoice {
    label: String,
    tool: String,
    selected: bool,
}

#[derive(Clone, Debug)]
struct TrialContenderEditor {
    checkpoint_id: String,
    session_id: String,
    prompt: String,
    verification_command: String,
    choices: Vec<TrialToolChoice>,
    cursor: usize,
}

#[derive(Clone, Debug)]
struct TrialVerificationEditor {
    checkpoint_id: String,
    session_id: String,
    prompt: String,
    command: String,
}

#[derive(Clone, Debug)]
struct CampaignTaskEditor {
    title: String,
    session_id: Option<String>,
}

#[derive(Clone, Debug)]
struct CampaignDependencyChoice {
    task_id: String,
    title: String,
    selected: bool,
}

#[derive(Clone, Debug)]
struct CampaignDependencyEditor {
    campaign_id: String,
    title: String,
    session_id: Option<String>,
    choices: Vec<CampaignDependencyChoice>,
    cursor: usize,
}

/// One rendered row: a display line plus the session it belongs to, so `Enter`
/// can jump without the renderer re-deriving ownership.
struct Row {
    line: String,
    accent: Option<Color>,
    attention_id: Option<String>,
    session_id: Option<String>,
    checkpoint_id: Option<String>,
    campaign_id: Option<String>,
    campaign_task_id: Option<String>,
    trial_id: Option<String>,
    trial_contender_id: Option<String>,
}

pub struct FleetDialog {
    tab: FleetTab,
    selected: usize,
    scroll: usize,
    visible_rows: usize,
    filter: String,
    filtering: bool,
    /// Rows for the current (snapshot version, panel, filter). Rebuilt only
    /// when one of those changes, so a repainted frame formats nothing.
    cached: Vec<Row>,
    cache_key: Option<(u64, FleetTab, String)>,
    checkpoint_target: Option<CheckpointTarget>,
    checkpoint_label: Option<String>,
    checkpoints_requested: bool,
    trial_tools: Vec<String>,
    trial_prompt: Option<TrialPromptEditor>,
    trial_verification: Option<TrialVerificationEditor>,
    trial_contenders: Option<TrialContenderEditor>,
    trials_requested: bool,
    campaigns_requested: bool,
    campaign_task: Option<CampaignTaskEditor>,
    campaign_dependencies: Option<CampaignDependencyEditor>,
}

impl Default for FleetDialog {
    fn default() -> Self {
        Self::new()
    }
}

impl FleetDialog {
    pub fn new() -> Self {
        Self {
            tab: FleetTab::Attention,
            selected: 0,
            scroll: 0,
            visible_rows: 1,
            filter: String::new(),
            filtering: false,
            cached: Vec::new(),
            cache_key: None,
            checkpoint_target: None,
            checkpoint_label: None,
            checkpoints_requested: false,
            trial_tools: Vec::new(),
            trial_prompt: None,
            trial_verification: None,
            trial_contenders: None,
            trials_requested: false,
            campaigns_requested: false,
            campaign_task: None,
            campaign_dependencies: None,
        }
    }

    pub(crate) fn with_checkpoint_target(mut self, target: Option<CheckpointTarget>) -> Self {
        self.checkpoint_target = target;
        self
    }

    pub(crate) fn with_trial_tools(mut self, tools: Vec<String>) -> Self {
        self.trial_tools = tools;
        self
    }

    fn select_tab(&mut self, delta: isize) {
        let count = FleetTab::ALL.len() as isize;
        let current = FleetTab::ALL
            .iter()
            .position(|tab| *tab == self.tab)
            .unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(count) as usize;
        self.tab = FleetTab::ALL[next];
        self.selected = 0;
        self.scroll = 0;
    }

    pub(crate) fn handle_key(
        &mut self,
        key: KeyEvent,
        snapshot: &FleetSnapshot,
    ) -> FleetDialogResult {
        if let Some(editor) = &mut self.campaign_dependencies {
            match key.code {
                KeyCode::Esc => {
                    let editor = self
                        .campaign_dependencies
                        .take()
                        .expect("campaign dependency editor exists");
                    self.campaign_task = Some(CampaignTaskEditor {
                        title: editor.title,
                        session_id: editor.session_id,
                    });
                    return FleetDialogResult::Continue;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    editor.cursor = editor.cursor.saturating_sub(1);
                    return FleetDialogResult::Continue;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    editor.cursor = (editor.cursor + 1).min(editor.choices.len().saturating_sub(1));
                    return FleetDialogResult::Continue;
                }
                KeyCode::Char(' ') => {
                    if let Some(choice) = editor.choices.get_mut(editor.cursor) {
                        choice.selected = !choice.selected;
                    }
                    return FleetDialogResult::Continue;
                }
                KeyCode::Enter => {
                    let editor = self
                        .campaign_dependencies
                        .take()
                        .expect("campaign dependency editor exists");
                    return FleetDialogResult::CreateCampaignTask {
                        campaign_id: Some(editor.campaign_id),
                        title: editor.title,
                        dependency_ids: editor
                            .choices
                            .into_iter()
                            .filter(|choice| choice.selected)
                            .map(|choice| choice.task_id)
                            .collect(),
                        session_id: editor.session_id,
                    };
                }
                _ => return FleetDialogResult::Continue,
            }
        }
        if let Some(editor) = &mut self.campaign_task {
            match key.code {
                KeyCode::Esc => {
                    self.campaign_task = None;
                    return FleetDialogResult::Continue;
                }
                KeyCode::Enter if !editor.title.trim().is_empty() => {
                    let editor = self
                        .campaign_task
                        .take()
                        .expect("campaign task editor exists");
                    let campaign_id = snapshot
                        .campaign_tasks
                        .first()
                        .map(|task| task.campaign_id.clone());
                    let choices: Vec<_> = campaign_id
                        .as_ref()
                        .into_iter()
                        .flat_map(|campaign_id| {
                            snapshot
                                .campaign_tasks
                                .iter()
                                .filter(move |task| &task.campaign_id == campaign_id)
                        })
                        .map(|task| CampaignDependencyChoice {
                            task_id: task.id.clone(),
                            title: task.title.clone(),
                            selected: false,
                        })
                        .collect();
                    if let Some(campaign_id) = campaign_id.clone().filter(|_| !choices.is_empty()) {
                        self.campaign_dependencies = Some(CampaignDependencyEditor {
                            campaign_id,
                            title: editor.title.trim().to_string(),
                            session_id: editor.session_id,
                            choices,
                            cursor: 0,
                        });
                        return FleetDialogResult::Continue;
                    }
                    return FleetDialogResult::CreateCampaignTask {
                        campaign_id,
                        title: editor.title.trim().to_string(),
                        dependency_ids: Vec::new(),
                        session_id: editor.session_id,
                    };
                }
                KeyCode::Backspace => {
                    editor.title.pop();
                    return FleetDialogResult::Continue;
                }
                KeyCode::Char(character)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && editor.title.chars().count()
                            < crate::campaigns::MAX_CAMPAIGN_TASK_TITLE_CHARS =>
                {
                    editor.title.push(character);
                    return FleetDialogResult::Continue;
                }
                _ => return FleetDialogResult::Continue,
            }
        }
        if let Some(editor) = &mut self.trial_contenders {
            match key.code {
                KeyCode::Esc => {
                    let editor = self.trial_contenders.take().expect("editor exists");
                    self.trial_verification = Some(TrialVerificationEditor {
                        checkpoint_id: editor.checkpoint_id,
                        session_id: editor.session_id,
                        prompt: editor.prompt,
                        command: editor.verification_command,
                    });
                    return FleetDialogResult::Continue;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    editor.cursor = editor.cursor.saturating_sub(1);
                    return FleetDialogResult::Continue;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    editor.cursor = (editor.cursor + 1).min(editor.choices.len().saturating_sub(1));
                    return FleetDialogResult::Continue;
                }
                KeyCode::Char(' ') => {
                    let selected = editor
                        .choices
                        .iter()
                        .filter(|choice| choice.selected)
                        .count();
                    if let Some(choice) = editor.choices.get_mut(editor.cursor) {
                        if choice.selected || selected < crate::trials::MAX_TRIAL_CONTENDERS {
                            choice.selected = !choice.selected;
                        }
                    }
                    return FleetDialogResult::Continue;
                }
                KeyCode::Enter
                    if (2..=crate::trials::MAX_TRIAL_CONTENDERS).contains(
                        &editor
                            .choices
                            .iter()
                            .filter(|choice| choice.selected)
                            .count(),
                    ) =>
                {
                    let editor = self.trial_contenders.take().expect("editor exists");
                    return FleetDialogResult::CreateTrial {
                        checkpoint_id: editor.checkpoint_id,
                        session_id: editor.session_id,
                        prompt: editor.prompt,
                        verification_command: editor.verification_command,
                        tools: editor
                            .choices
                            .into_iter()
                            .filter(|choice| choice.selected)
                            .map(|choice| choice.tool)
                            .collect(),
                    };
                }
                _ => return FleetDialogResult::Continue,
            }
        }
        if let Some(editor) = &mut self.trial_verification {
            match key.code {
                KeyCode::Esc => {
                    let editor = self.trial_verification.take().expect("editor exists");
                    self.trial_prompt = Some(TrialPromptEditor {
                        checkpoint_id: editor.checkpoint_id,
                        session_id: editor.session_id,
                        prompt: editor.prompt,
                    });
                    return FleetDialogResult::Continue;
                }
                KeyCode::Enter if !editor.command.trim().is_empty() => {
                    let editor = self.trial_verification.take().expect("editor exists");
                    let mut tools = self.trial_tools.clone();
                    if tools.len() == 1 {
                        tools.push(tools[0].clone());
                    }
                    let choices = tools
                        .into_iter()
                        .enumerate()
                        .map(|(index, tool)| TrialToolChoice {
                            label: if self.trial_tools.len() == 1 {
                                format!("{} {}", tool, if index == 0 { "A" } else { "B" })
                            } else {
                                tool.clone()
                            },
                            tool,
                            selected: index < 2,
                        })
                        .collect();
                    self.trial_contenders = Some(TrialContenderEditor {
                        checkpoint_id: editor.checkpoint_id,
                        session_id: editor.session_id,
                        prompt: editor.prompt,
                        verification_command: editor.command.trim().to_string(),
                        choices,
                        cursor: 0,
                    });
                    return FleetDialogResult::Continue;
                }
                KeyCode::Backspace => {
                    editor.command.pop();
                    return FleetDialogResult::Continue;
                }
                KeyCode::Char(character)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && editor.command.len() + character.len_utf8()
                            <= crate::trials::MAX_VERIFICATION_COMMAND_BYTES =>
                {
                    editor.command.push(character);
                    return FleetDialogResult::Continue;
                }
                _ => return FleetDialogResult::Continue,
            }
        }
        if let Some(editor) = &mut self.trial_prompt {
            match key.code {
                KeyCode::Esc => {
                    self.trial_prompt = None;
                    return FleetDialogResult::Continue;
                }
                KeyCode::Enter if !editor.prompt.trim().is_empty() => {
                    let editor = self.trial_prompt.take().expect("editor exists");
                    self.trial_verification = Some(TrialVerificationEditor {
                        checkpoint_id: editor.checkpoint_id,
                        session_id: editor.session_id,
                        prompt: editor.prompt.trim().to_string(),
                        command: String::new(),
                    });
                    return FleetDialogResult::Continue;
                }
                KeyCode::Backspace => {
                    editor.prompt.pop();
                    return FleetDialogResult::Continue;
                }
                KeyCode::Char(character)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && editor.prompt.len() + character.len_utf8()
                            <= crate::trials::MAX_TRIAL_PROMPT_BYTES =>
                {
                    editor.prompt.push(character);
                    return FleetDialogResult::Continue;
                }
                _ => return FleetDialogResult::Continue,
            }
        }
        if let Some(label) = &mut self.checkpoint_label {
            match key.code {
                KeyCode::Esc => {
                    self.checkpoint_label = None;
                    return FleetDialogResult::Continue;
                }
                KeyCode::Enter => {
                    let label = self.checkpoint_label.take().unwrap_or_default();
                    let Some(target) = &self.checkpoint_target else {
                        return FleetDialogResult::Continue;
                    };
                    return FleetDialogResult::CaptureCheckpoint {
                        session_id: target.session_id.clone(),
                        label,
                    };
                }
                KeyCode::Backspace => {
                    label.pop();
                    return FleetDialogResult::Continue;
                }
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && label.chars().count()
                            < crate::checkpoints::MAX_CHECKPOINT_LABEL_CHARS =>
                {
                    label.push(c);
                    return FleetDialogResult::Continue;
                }
                _ => return FleetDialogResult::Continue,
            }
        }
        let row_count = self.rows(snapshot).len();
        if self.filtering {
            match key.code {
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                    self.selected = 0;
                    self.scroll = 0;
                    return FleetDialogResult::Continue;
                }
                KeyCode::Enter => {
                    self.filtering = false;
                    return FleetDialogResult::Continue;
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.selected = 0;
                    self.scroll = 0;
                    return FleetDialogResult::Continue;
                }
                // While the filter owns input, every printable key is text.
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.filter.push(c);
                    self.selected = 0;
                    self.scroll = 0;
                    return FleetDialogResult::Continue;
                }
                _ => {}
            }
        }

        match key.code {
            // Esc clears an applied filter before it closes the panel.
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.selected = 0;
                self.scroll = 0;
                FleetDialogResult::Continue
            }
            KeyCode::Esc => FleetDialogResult::Cancel,
            KeyCode::Char('/') => {
                self.filtering = true;
                FleetDialogResult::Continue
            }
            KeyCode::Char('r') if self.tab == FleetTab::Conflicts => {
                FleetDialogResult::RefreshConflicts
            }
            KeyCode::Char('a') if self.tab == FleetTab::Attention => self
                .selected_attention(snapshot)
                .map(|item_id| FleetDialogResult::AcknowledgeAttention { item_id })
                .unwrap_or(FleetDialogResult::Continue),
            KeyCode::Char('s') if self.tab == FleetTab::Conflicts => self
                .selected_session(snapshot)
                .map(|session_id| FleetDialogResult::CoordinateConflict { session_id })
                .unwrap_or(FleetDialogResult::Continue),
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.select_tab(1);
                self.tab_result()
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.select_tab(-1);
                self.tab_result()
            }
            KeyCode::Char('c') if self.tab == FleetTab::Checkpoints => {
                if self.checkpoint_target.is_some() {
                    self.checkpoint_label = Some(String::new());
                }
                FleetDialogResult::Continue
            }
            KeyCode::Char('f') if self.tab == FleetTab::Checkpoints => {
                let selected = self.selected;
                let target = self
                    .rows(snapshot)
                    .get(selected)
                    .and_then(|row| Some((row.checkpoint_id.clone()?, row.session_id.clone()?)));
                target
                    .map(
                        |(checkpoint_id, session_id)| FleetDialogResult::ForkCheckpoint {
                            checkpoint_id,
                            session_id,
                        },
                    )
                    .unwrap_or(FleetDialogResult::Continue)
            }
            KeyCode::Char('i') if self.tab == FleetTab::Checkpoints => self
                .selected_checkpoint(snapshot)
                .map(|checkpoint_id| FleetDialogResult::InspectCheckpoint { checkpoint_id })
                .unwrap_or(FleetDialogResult::Continue),
            KeyCode::Char('e') if self.tab == FleetTab::Checkpoints => self
                .selected_checkpoint(snapshot)
                .map(|checkpoint_id| FleetDialogResult::ExportCheckpoint { checkpoint_id })
                .unwrap_or(FleetDialogResult::Continue),
            KeyCode::Char('d') if self.tab == FleetTab::Checkpoints => self
                .selected_checkpoint(snapshot)
                .map(|checkpoint_id| FleetDialogResult::DeleteCheckpoint { checkpoint_id })
                .unwrap_or(FleetDialogResult::Continue),
            KeyCode::Char('r') if self.tab == FleetTab::Checkpoints => {
                let selected = self.selected;
                let target = self
                    .rows(snapshot)
                    .get(selected)
                    .and_then(|row| Some((row.checkpoint_id.clone()?, row.session_id.clone()?)));
                target
                    .map(
                        |(checkpoint_id, session_id)| FleetDialogResult::RestoreCheckpoint {
                            checkpoint_id,
                            session_id,
                        },
                    )
                    .unwrap_or(FleetDialogResult::Continue)
            }
            KeyCode::Char('t') if self.tab == FleetTab::Checkpoints => {
                let selected = self.selected;
                let target = self
                    .rows(snapshot)
                    .get(selected)
                    .and_then(|row| Some((row.checkpoint_id.clone()?, row.session_id.clone()?)));
                if let Some((checkpoint_id, session_id)) = target {
                    if !self.trial_tools.is_empty() {
                        self.trial_prompt = Some(TrialPromptEditor {
                            checkpoint_id,
                            session_id,
                            prompt: String::new(),
                        });
                    }
                }
                FleetDialogResult::Continue
            }
            KeyCode::Char('n') if self.tab == FleetTab::Campaign => {
                self.campaign_task = Some(CampaignTaskEditor {
                    title: String::new(),
                    session_id: self
                        .checkpoint_target
                        .as_ref()
                        .map(|target| target.session_id.clone()),
                });
                FleetDialogResult::Continue
            }
            KeyCode::Char('a') if self.tab == FleetTab::Campaign => self
                .selected_campaign_task(snapshot)
                .map(
                    |(campaign_id, task_id)| FleetDialogResult::UpdateCampaignTask {
                        campaign_id,
                        task_id,
                        state: crate::campaigns::CampaignTaskState::Active,
                    },
                )
                .unwrap_or(FleetDialogResult::Continue),
            KeyCode::Char('w') if self.tab == FleetTab::Campaign => self
                .selected_campaign_task(snapshot)
                .map(
                    |(campaign_id, task_id)| FleetDialogResult::UpdateCampaignTask {
                        campaign_id,
                        task_id,
                        state: crate::campaigns::CampaignTaskState::Waiting,
                    },
                )
                .unwrap_or(FleetDialogResult::Continue),
            KeyCode::Char('v') if self.tab == FleetTab::Campaign => self
                .selected_campaign_task(snapshot)
                .map(
                    |(campaign_id, task_id)| FleetDialogResult::UpdateCampaignTask {
                        campaign_id,
                        task_id,
                        state: crate::campaigns::CampaignTaskState::Review,
                    },
                )
                .unwrap_or(FleetDialogResult::Continue),
            KeyCode::Char('m') if self.tab == FleetTab::Campaign => self
                .selected_campaign_task(snapshot)
                .map(
                    |(campaign_id, task_id)| FleetDialogResult::UpdateCampaignTask {
                        campaign_id,
                        task_id,
                        state: crate::campaigns::CampaignTaskState::Done,
                    },
                )
                .unwrap_or(FleetDialogResult::Continue),
            KeyCode::Char('f') if self.tab == FleetTab::Campaign => self
                .selected_campaign_task(snapshot)
                .map(
                    |(campaign_id, task_id)| FleetDialogResult::UpdateCampaignTask {
                        campaign_id,
                        task_id,
                        state: crate::campaigns::CampaignTaskState::Failed,
                    },
                )
                .unwrap_or(FleetDialogResult::Continue),
            KeyCode::Char('x') if self.tab == FleetTab::Campaign => self
                .selected_campaign_task(snapshot)
                .map(
                    |(campaign_id, task_id)| FleetDialogResult::UpdateCampaignTask {
                        campaign_id,
                        task_id,
                        state: crate::campaigns::CampaignTaskState::Cancelled,
                    },
                )
                .unwrap_or(FleetDialogResult::Continue),
            KeyCode::Char('y') if self.tab == FleetTab::Trials => self
                .selected_trial_contender_with_state(snapshot)
                .filter(|(_, _, state)| {
                    matches!(state, ContenderState::Failed | ContenderState::Cancelled)
                })
                .map(
                    |(trial_id, contender_id, _)| FleetDialogResult::RetryTrialContender {
                        trial_id,
                        contender_id,
                    },
                )
                .unwrap_or(FleetDialogResult::Continue),
            KeyCode::Char('x') if self.tab == FleetTab::Trials => self
                .selected_trial_with_state(snapshot)
                .filter(|(_, state)| matches!(state, TrialState::Launching | TrialState::Running))
                .map(|(trial_id, _)| FleetDialogResult::CancelTrial { trial_id })
                .unwrap_or(FleetDialogResult::Continue),
            KeyCode::Char('w') if self.tab == FleetTab::Trials => self
                .selected_trial_contender_with_state(snapshot)
                .filter(|(_, _, state)| *state == ContenderState::Completed)
                .map(
                    |(trial_id, contender_id, _)| FleetDialogResult::RetainTrialWinner {
                        trial_id,
                        contender_id,
                    },
                )
                .unwrap_or(FleetDialogResult::Continue),
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                FleetDialogResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if row_count > 0 {
                    self.selected = (self.selected + 1).min(row_count - 1);
                }
                FleetDialogResult::Continue
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.selected = 0;
                FleetDialogResult::Continue
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = row_count.saturating_sub(1);
                FleetDialogResult::Continue
            }
            KeyCode::Enter => {
                let selected = self.selected;
                let target = self
                    .rows(snapshot)
                    .get(selected)
                    .and_then(|row| row.session_id.clone());
                target
                    .map(FleetDialogResult::Jump)
                    .unwrap_or(FleetDialogResult::Continue)
            }
            _ => FleetDialogResult::Continue,
        }
    }

    fn tab_result(&mut self) -> FleetDialogResult {
        if self.tab == FleetTab::Campaign && !self.campaigns_requested {
            self.campaigns_requested = true;
            FleetDialogResult::LoadCampaigns
        } else if self.tab == FleetTab::Checkpoints && !self.checkpoints_requested {
            self.checkpoints_requested = true;
            FleetDialogResult::LoadCheckpoints
        } else if self.tab == FleetTab::Trials && !self.trials_requested {
            self.trials_requested = true;
            FleetDialogResult::LoadTrials
        } else {
            FleetDialogResult::Continue
        }
    }

    fn selected_checkpoint(&mut self, snapshot: &FleetSnapshot) -> Option<String> {
        let selected = self.selected;
        self.rows(snapshot)
            .get(selected)
            .and_then(|row| row.checkpoint_id.clone())
    }

    fn selected_attention(&mut self, snapshot: &FleetSnapshot) -> Option<String> {
        let selected = self.selected;
        self.rows(snapshot)
            .get(selected)
            .and_then(|row| row.attention_id.clone())
    }

    fn selected_session(&mut self, snapshot: &FleetSnapshot) -> Option<String> {
        let selected = self.selected;
        self.rows(snapshot)
            .get(selected)
            .and_then(|row| row.session_id.clone())
    }

    fn selected_campaign_task(&mut self, snapshot: &FleetSnapshot) -> Option<(String, String)> {
        let selected = self.selected;
        let row = self.rows(snapshot).get(selected)?;
        Some((row.campaign_id.clone()?, row.campaign_task_id.clone()?))
    }

    fn selected_trial(&mut self, snapshot: &FleetSnapshot) -> Option<String> {
        let selected = self.selected;
        self.rows(snapshot)
            .get(selected)
            .and_then(|row| row.trial_id.clone())
    }

    fn selected_trial_with_state(
        &mut self,
        snapshot: &FleetSnapshot,
    ) -> Option<(String, TrialState)> {
        let trial_id = self.selected_trial(snapshot)?;
        let state = snapshot
            .trials
            .iter()
            .find(|trial| trial.id == trial_id)?
            .state;
        Some((trial_id, state))
    }

    fn selected_trial_contender(&mut self, snapshot: &FleetSnapshot) -> Option<(String, String)> {
        let selected = self.selected;
        let row = self.rows(snapshot).get(selected)?;
        Some((row.trial_id.clone()?, row.trial_contender_id.clone()?))
    }

    fn selected_trial_contender_with_state(
        &mut self,
        snapshot: &FleetSnapshot,
    ) -> Option<(String, String, ContenderState)> {
        let (trial_id, contender_id) = self.selected_trial_contender(snapshot)?;
        let state = snapshot
            .trials
            .iter()
            .find(|trial| trial.id == trial_id)?
            .contenders
            .iter()
            .find(|contender| contender.id == contender_id)?
            .state;
        Some((trial_id, contender_id, state))
    }

    pub(crate) fn scroll_wheel(&mut self, down: bool) {
        if let Some(editor) = &mut self.trial_contenders {
            if down {
                editor.cursor = (editor.cursor + 3).min(editor.choices.len().saturating_sub(1));
            } else {
                editor.cursor = editor.cursor.saturating_sub(3);
            }
            return;
        }
        if down {
            self.selected = self.selected.saturating_add(3);
        } else {
            self.selected = self.selected.saturating_sub(3);
        }
    }

    pub(crate) fn handle_paste(&mut self, text: &str) {
        if let Some(editor) = &mut self.campaign_task {
            let remaining = crate::campaigns::MAX_CAMPAIGN_TASK_TITLE_CHARS
                .saturating_sub(editor.title.chars().count());
            editor.title.extend(text.chars().take(remaining));
            return;
        }
        if let Some(editor) = &mut self.trial_prompt {
            for character in text.chars() {
                if editor.prompt.len() + character.len_utf8()
                    > crate::trials::MAX_TRIAL_PROMPT_BYTES
                {
                    break;
                }
                editor.prompt.push(character);
            }
            return;
        }
        if let Some(editor) = &mut self.trial_verification {
            for character in text.chars() {
                if editor.command.len() + character.len_utf8()
                    > crate::trials::MAX_VERIFICATION_COMMAND_BYTES
                {
                    break;
                }
                editor.command.push(character);
            }
            return;
        }
        if let Some(label) = &mut self.checkpoint_label {
            let remaining = crate::checkpoints::MAX_CHECKPOINT_LABEL_CHARS
                .saturating_sub(label.chars().count());
            label.extend(text.chars().take(remaining));
            return;
        }
        if self.filtering {
            self.filter.push_str(text);
            self.selected = 0;
            self.scroll = 0;
        }
    }

    fn rows(&mut self, snapshot: &FleetSnapshot) -> &[Row] {
        let key = (snapshot.version, self.tab, self.filter.clone());
        if self.cache_key.as_ref() == Some(&key) {
            return &self.cached;
        }
        let rows = match self.tab {
            FleetTab::Attention => attention_rows(snapshot),
            FleetTab::Conflicts => conflict_rows(snapshot),
            FleetTab::Readiness => readiness_rows(snapshot),
            FleetTab::Campaign => campaign_rows(snapshot),
            FleetTab::Budget => budget_rows(snapshot),
            FleetTab::Trials => trial_rows(snapshot),
            FleetTab::Checkpoints => checkpoint_rows(snapshot),
        };
        self.cached = if self.filter.is_empty() {
            rows
        } else {
            let needle = self.filter.to_lowercase();
            rows.into_iter()
                .filter(|row| row.line.to_lowercase().contains(&needle))
                .collect()
        };
        self.cache_key = Some(key);
        &self.cached
    }

    pub(crate) fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        snapshot: &FleetSnapshot,
    ) {
        let row_count = if let Some(editor) = &self.campaign_dependencies {
            editor.choices.len()
        } else if let Some(editor) = &self.trial_contenders {
            editor.choices.len()
        } else {
            self.rows(snapshot).len()
        };
        let width = area.width.saturating_sub(4).clamp(28, 110);
        let height = area.height.saturating_sub(2).clamp(10, 34);
        let dialog_area = super::centered_rect(area, width, height);
        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(" Fleet operations ")
            .title_style(Style::default().fg(theme.title).bold())
            .title_bottom(self.footer(theme).right_aligned());
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .margin(1)
        .split(inner);

        frame.render_widget(Paragraph::new(self.tab_bar(theme)), chunks[0]);
        frame.render_widget(
            Paragraph::new(self.status_line(snapshot)).style(Style::default().fg(theme.dimmed)),
            chunks[1],
        );

        self.visible_rows = chunks[2].height.max(1) as usize;
        if row_count == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(row_count - 1);
        }
        let navigation_index = self
            .campaign_dependencies
            .as_ref()
            .map(|editor| editor.cursor)
            .or_else(|| self.trial_contenders.as_ref().map(|editor| editor.cursor))
            .unwrap_or(self.selected);
        self.scroll = self
            .scroll
            .min(navigation_index)
            .max(navigation_index.saturating_sub(self.visible_rows.saturating_sub(1)));

        if let Some(editor) = &self.campaign_task {
            let title = if editor.title.is_empty() {
                "_"
            } else {
                editor.title.as_str()
            };
            frame.render_widget(
                Paragraph::new(title)
                    .style(Style::default().fg(theme.text))
                    .wrap(Wrap { trim: false }),
                chunks[2],
            );
            return;
        }

        if let Some(editor) = &self.campaign_dependencies {
            let lines = editor
                .choices
                .iter()
                .enumerate()
                .skip(self.scroll)
                .take(self.visible_rows)
                .map(|(index, choice)| {
                    let marker = if choice.selected { "[x]" } else { "[ ]" };
                    let style = if index == editor.cursor {
                        Style::default().fg(theme.text).bg(theme.selection).bold()
                    } else {
                        Style::default().fg(theme.text)
                    };
                    Line::from(Span::styled(format!("{marker} {}", choice.title), style))
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines), chunks[2]);
            return;
        }

        if let Some(editor) = &self.trial_prompt {
            let prompt = if editor.prompt.is_empty() {
                "_"
            } else {
                editor.prompt.as_str()
            };
            frame.render_widget(
                Paragraph::new(prompt)
                    .style(Style::default().fg(theme.text))
                    .wrap(Wrap { trim: false }),
                chunks[2],
            );
            return;
        }

        if let Some(editor) = &self.trial_verification {
            let command = if editor.command.is_empty() {
                "_"
            } else {
                editor.command.as_str()
            };
            frame.render_widget(
                Paragraph::new(command)
                    .style(Style::default().fg(theme.text))
                    .wrap(Wrap { trim: false }),
                chunks[2],
            );
            return;
        }

        if let Some(editor) = &self.trial_contenders {
            let lines = editor
                .choices
                .iter()
                .enumerate()
                .skip(self.scroll)
                .take(self.visible_rows)
                .map(|(index, choice)| {
                    let marker = if choice.selected { "[x]" } else { "[ ]" };
                    let style = if index == editor.cursor {
                        Style::default().fg(theme.text).bg(theme.selection).bold()
                    } else {
                        Style::default().fg(theme.text)
                    };
                    Line::from(Span::styled(format!("{marker} {}", choice.label), style))
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines), chunks[2]);
            return;
        }

        if row_count == 0 {
            frame.render_widget(
                Paragraph::new(empty_message(
                    self.tab,
                    snapshot,
                    &self.filter,
                    self.checkpoint_target.is_some(),
                ))
                .style(Style::default().fg(theme.dimmed))
                .wrap(Wrap { trim: true }),
                chunks[2],
            );
            return;
        }

        let selected = self.selected;
        let lines: Vec<Line> = self
            .cached
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(self.visible_rows)
            .map(|(index, row)| {
                let mut style = Style::default().fg(row.accent.unwrap_or(theme.text));
                if index == selected {
                    style = style.bg(theme.selection).bold();
                }
                Line::from(Span::styled(row.line.clone(), style))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), chunks[2]);
    }

    fn tab_bar(&self, theme: &Theme) -> Line<'static> {
        let mut spans = Vec::new();
        for tab in FleetTab::ALL {
            let style = if tab == self.tab {
                Style::default().fg(theme.accent).bold()
            } else {
                Style::default().fg(theme.dimmed)
            };
            spans.push(Span::styled(tab.label(), style));
            spans.push(Span::styled("  ", Style::default().fg(theme.dimmed)));
        }
        Line::from(spans)
    }

    fn footer(&self, theme: &Theme) -> Line<'static> {
        let accent = Style::default().fg(theme.accent);
        let dim = Style::default().fg(theme.dimmed);
        if self.campaign_task.is_some() {
            return Line::from(vec![
                Span::styled(" Enter", accent),
                Span::styled(" dependencies  ", dim),
                Span::styled("Esc", accent),
                Span::styled(" cancel ", dim),
            ]);
        }
        if self.campaign_dependencies.is_some() {
            return Line::from(vec![
                Span::styled(" Space", accent),
                Span::styled(" dependency  ", dim),
                Span::styled("j/k", accent),
                Span::styled(" move  ", dim),
                Span::styled("Enter", accent),
                Span::styled(" create  ", dim),
                Span::styled("Esc", accent),
                Span::styled(" back ", dim),
            ]);
        }
        if self.trial_prompt.is_some() {
            return Line::from(vec![
                Span::styled(" Enter", accent),
                Span::styled(" verification  ", dim),
                Span::styled("Esc", accent),
                Span::styled(" cancel ", dim),
            ]);
        }
        if self.trial_verification.is_some() {
            return Line::from(vec![
                Span::styled(" Enter", accent),
                Span::styled(" choose contenders  ", dim),
                Span::styled("Esc", accent),
                Span::styled(" back ", dim),
            ]);
        }
        if let Some(editor) = &self.trial_contenders {
            let selected = editor
                .choices
                .iter()
                .filter(|choice| choice.selected)
                .count();
            return Line::from(vec![
                Span::styled(" Space", accent),
                Span::styled(" toggle  ", dim),
                Span::styled("j/k", accent),
                Span::styled(" move  ", dim),
                Span::styled("Enter", accent),
                Span::styled(
                    if selected >= 2 {
                        " launch  "
                    } else {
                        " select 2+  "
                    },
                    dim,
                ),
                Span::styled("Esc", accent),
                Span::styled(" back ", dim),
            ]);
        }
        if self.checkpoint_label.is_some() {
            return Line::from(vec![
                Span::styled(" Enter", accent),
                Span::styled(" capture  ", dim),
                Span::styled("Esc", accent),
                Span::styled(" cancel label ", dim),
            ]);
        }
        let mut spans = vec![
            Span::styled(" Tab", accent),
            Span::styled(" panel  ", dim),
            Span::styled("j/k", accent),
            Span::styled(" move  ", dim),
            Span::styled("Enter", accent),
            Span::styled(" go  ", dim),
        ];
        if self.tab == FleetTab::Checkpoints && self.checkpoint_target.is_some() {
            spans.extend([Span::styled("c", accent), Span::styled(" capture  ", dim)]);
        }
        if self.tab == FleetTab::Conflicts {
            spans.extend([
                Span::styled("r", accent),
                Span::styled(" refresh  ", dim),
                Span::styled("s", accent),
                Span::styled(" coordinate  ", dim),
            ]);
        }
        if self.tab == FleetTab::Attention {
            spans.extend([
                Span::styled("a", accent),
                Span::styled(" acknowledge  ", dim),
            ]);
        }
        if self.tab == FleetTab::Checkpoints {
            spans.extend([
                Span::styled("i/e", accent),
                Span::styled(" inspect/export  ", dim),
                Span::styled("r", accent),
                Span::styled(" restore  ", dim),
                Span::styled("d", accent),
                Span::styled(" delete  ", dim),
                Span::styled("f", accent),
                Span::styled(" fork code  ", dim),
                Span::styled("t", accent),
                Span::styled(" trial  ", dim),
            ]);
        }
        if self.tab == FleetTab::Campaign {
            spans.extend([
                Span::styled("n", accent),
                Span::styled(" new  ", dim),
                Span::styled("a", accent),
                Span::styled(" active  ", dim),
                Span::styled("w/v/m", accent),
                Span::styled(" wait/review/done  ", dim),
                Span::styled("f/x", accent),
                Span::styled(" fail/cancel  ", dim),
            ]);
        }
        if self.tab == FleetTab::Trials {
            spans.extend([
                Span::styled("y", accent),
                Span::styled(" retry  ", dim),
                Span::styled("x", accent),
                Span::styled(" cancel  ", dim),
                Span::styled("w", accent),
                Span::styled(" retain winner  ", dim),
            ]);
        }
        spans.extend([
            Span::styled("/", accent),
            Span::styled(" filter  ", dim),
            Span::styled("Esc", accent),
            Span::styled(" close ", dim),
        ]);
        Line::from(spans)
    }

    fn status_line(&self, snapshot: &FleetSnapshot) -> String {
        if let Some(editor) = &self.campaign_task {
            return format!(
                "New campaign task · {} character{} · Enter continues",
                editor.title.chars().count(),
                if editor.title.chars().count() == 1 {
                    ""
                } else {
                    "s"
                }
            );
        }
        if let Some(editor) = &self.campaign_dependencies {
            return format!(
                "Choose blockers for '{}' · {} selected · Enter creates",
                truncate(&editor.title, 36),
                editor
                    .choices
                    .iter()
                    .filter(|choice| choice.selected)
                    .count()
            );
        }
        if let Some(editor) = &self.trial_prompt {
            return format!(
                "Trial prompt · {} byte{} · Enter continues",
                editor.prompt.len(),
                if editor.prompt.len() == 1 { "" } else { "s" }
            );
        }
        if let Some(editor) = &self.trial_verification {
            return format!(
                "Verification command · {} byte{} · runs once per completed contender",
                editor.command.len(),
                if editor.command.len() == 1 { "" } else { "s" }
            );
        }
        if let Some(editor) = &self.trial_contenders {
            let selected = editor
                .choices
                .iter()
                .filter(|choice| choice.selected)
                .count();
            return format!(
                "Choose 2–{} contenders · {selected} selected · each starts from the same checkpoint",
                crate::trials::MAX_TRIAL_CONTENDERS
            );
        }
        if let Some(label) = &self.checkpoint_label {
            let target = self
                .checkpoint_target
                .as_ref()
                .map(|target| target.title.as_str())
                .unwrap_or("selected session");
            let label = if label.is_empty() { "_" } else { label };
            return format!("Checkpoint {target} · label: {label}");
        }
        let mut line = status_line(snapshot, &self.filter);
        if self.tab == FleetTab::Campaign {
            let history = match &snapshot.campaign_load {
                CampaignLoadState::Dormant => "history not loaded".to_string(),
                CampaignLoadState::Loading => "loading campaigns".to_string(),
                CampaignLoadState::Ready { loaded, errors } if !errors.is_empty() => {
                    format!("{loaded} loaded, {} manifest error(s)", errors.len())
                }
                CampaignLoadState::Ready { loaded, .. } => format!("{loaded} loaded"),
                CampaignLoadState::Failed(error) => format!("campaigns unavailable: {error}"),
            };
            line.push_str(&format!(" · campaigns: {history}"));
        } else if self.tab == FleetTab::Checkpoints {
            let history = match &snapshot.checkpoint_load {
                CheckpointLoadState::Dormant => "history not loaded".to_string(),
                CheckpointLoadState::Loading => "loading history".to_string(),
                CheckpointLoadState::Ready { loaded, errors } if !errors.is_empty() => {
                    format!("{loaded} loaded, {} manifest error(s)", errors.len())
                }
                CheckpointLoadState::Ready { loaded, .. } => format!("{loaded} loaded"),
                CheckpointLoadState::Failed(error) => format!("history unavailable: {error}"),
            };
            line.push_str(&format!(" · checkpoints: {history}"));
            if self.checkpoint_target.is_none() {
                line.push_str(" · select a session before opening to capture");
            }
        } else if self.tab == FleetTab::Trials {
            let history = match &snapshot.trial_load {
                TrialLoadState::Dormant => "history not loaded".to_string(),
                TrialLoadState::Loading => "loading history".to_string(),
                TrialLoadState::Ready { loaded, errors } if !errors.is_empty() => {
                    format!("{loaded} loaded, {} manifest error(s)", errors.len())
                }
                TrialLoadState::Ready { loaded, .. } => format!("{loaded} loaded"),
                TrialLoadState::Failed(error) => format!("history unavailable: {error}"),
            };
            line.push_str(&format!(" · trials: {history}"));
        }
        line
    }
}

fn status_line(snapshot: &FleetSnapshot, filter: &str) -> String {
    let scan = match &snapshot.scan {
        ScanState::Dormant => "repository scan: not started".to_string(),
        ScanState::Scanning { pending } => format!("repository scan: {pending} in flight"),
        ScanState::Ready { scanned, skipped } if *skipped > 0 => {
            format!("repository scan: {scanned} scanned, {skipped} unavailable")
        }
        ScanState::Ready { scanned, .. } => format!("repository scan: {scanned} scanned"),
    };
    let mut line = format!("{} sessions · {scan}", snapshot.sessions);
    if !filter.is_empty() {
        line.push_str(&format!(" · filter \"{filter}\""));
    }
    line
}

fn empty_message(
    tab: FleetTab,
    snapshot: &FleetSnapshot,
    filter: &str,
    can_capture_checkpoint: bool,
) -> String {
    if !filter.is_empty() {
        return format!("Nothing in {} matches \"{filter}\".", tab.label());
    }
    match tab {
        FleetTab::Attention => "Nothing needs you right now.".to_string(),
        FleetTab::Conflicts => match &snapshot.scan {
            ScanState::Dormant => "No repository scan has run yet.".to_string(),
            ScanState::Scanning { .. } => "Scanning worktrees…".to_string(),
            ScanState::Ready { .. } => "No two sessions are changing the same file.".to_string(),
        },
        FleetTab::Readiness => "No sessions to evaluate.".to_string(),
        FleetTab::Campaign => match &snapshot.campaign_load {
            CampaignLoadState::Dormant => "Campaign history has not been loaded.".to_string(),
            CampaignLoadState::Loading => "Loading campaigns…".to_string(),
            CampaignLoadState::Ready { errors, .. } if !errors.is_empty() => {
                format!("No readable campaigns. {} manifest error(s).", errors.len())
            }
            CampaignLoadState::Ready { .. } => {
                "No campaign tasks. Press n to create the first task.".to_string()
            }
            CampaignLoadState::Failed(error) => format!("Campaign history unavailable: {error}"),
        },
        FleetTab::Budget => "No capacity information.".to_string(),
        FleetTab::Trials => match &snapshot.trial_load {
            TrialLoadState::Dormant => "Trial history has not been loaded.".to_string(),
            TrialLoadState::Loading => "Loading trial history…".to_string(),
            TrialLoadState::Ready { errors, .. } if !errors.is_empty() => {
                format!("No readable trials. {} manifest error(s).", errors.len())
            }
            TrialLoadState::Ready { .. } => {
                "No trials recorded. Select a checkpoint and press t to launch one.".to_string()
            }
            TrialLoadState::Failed(error) => format!("Trial history unavailable: {error}"),
        },
        FleetTab::Checkpoints => match &snapshot.checkpoint_load {
            CheckpointLoadState::Dormant => "Checkpoint history has not been loaded.".to_string(),
            CheckpointLoadState::Loading => "Loading checkpoint history…".to_string(),
            CheckpointLoadState::Ready { errors, .. } if !errors.is_empty() => {
                format!(
                    "No readable checkpoints. {} manifest error(s).",
                    errors.len()
                )
            }
            CheckpointLoadState::Ready { .. } => {
                if can_capture_checkpoint {
                    "No checkpoints recorded. Press c to capture the selected session.".to_string()
                } else {
                    "No checkpoints recorded. Select a session before reopening Fleet Operations to capture one."
                        .to_string()
                }
            }
            CheckpointLoadState::Failed(error) => {
                format!("Checkpoint history unavailable: {error}")
            }
        },
    }
}

fn attention_rows(snapshot: &FleetSnapshot) -> Vec<Row> {
    snapshot
        .attention
        .iter()
        .map(|item| Row {
            line: format!(
                "{:<9} {:<28} {}",
                reason_label(item.reason),
                truncate(&item.title, 28),
                item.summary
            ),
            accent: None,
            attention_id: Some(item.item_id.clone()),
            session_id: Some(item.session_id.clone()),
            checkpoint_id: None,
            campaign_id: None,
            campaign_task_id: None,
            trial_id: None,
            trial_contender_id: None,
        })
        .collect()
}

fn reason_label(reason: AttentionReason) -> &'static str {
    match reason {
        AttentionReason::Failed => "error",
        AttentionReason::Capacity => "capacity",
        AttentionReason::Approval => "approve",
        AttentionReason::Input => "input",
        AttentionReason::Conflict => "conflict",
        AttentionReason::CampaignReady => "ready",
        AttentionReason::Completed => "unread",
    }
}

fn conflict_rows(snapshot: &FleetSnapshot) -> Vec<Row> {
    let mut rows: Vec<Row> = snapshot
        .conflicts
        .iter()
        .map(|conflict| Row {
            line: format!(
                "{}/{}  ·  {}",
                conflict.repo,
                conflict.path,
                conflict.sessions.join(", ")
            ),
            accent: None,
            attention_id: None,
            session_id: conflict.session_ids.first().cloned(),
            checkpoint_id: None,
            campaign_id: None,
            campaign_task_id: None,
            trial_id: None,
            trial_contender_id: None,
        })
        .collect();
    // A session whose scan failed contributes no paths, so say so rather than
    // letting it read as a clean worktree.
    rows.extend(snapshot.scan_errors.iter().map(|error| Row {
        line: format!("unscanned  {error}"),
        accent: None,
        attention_id: None,
        session_id: None,
        checkpoint_id: None,
        campaign_id: None,
        campaign_task_id: None,
        trial_id: None,
        trial_contender_id: None,
    }));
    rows
}

fn readiness_rows(snapshot: &FleetSnapshot) -> Vec<Row> {
    snapshot
        .readiness
        .iter()
        .map(|row| {
            let evidence = row
                .evidence
                .iter()
                .map(|fact| {
                    format!(
                        "{}={}",
                        evidence_kind_label(fact.kind),
                        evidence_state_label(fact.state)
                    )
                })
                .collect::<Vec<_>>()
                .join(" ");
            Row {
                line: format!(
                    "{:<9} {:<24} {evidence}",
                    verdict_label(row.verdict),
                    truncate(&row.title, 24)
                ),
                accent: None,
                attention_id: None,
                session_id: Some(row.session_id.clone()),
                checkpoint_id: None,
                campaign_id: None,
                campaign_task_id: None,
                trial_id: None,
                trial_contender_id: None,
            }
        })
        .collect()
}

fn verdict_label(verdict: ReadinessVerdict) -> &'static str {
    match verdict {
        ReadinessVerdict::Ready => "ready",
        ReadinessVerdict::Running => "running",
        ReadinessVerdict::Blocked => "blocked",
        ReadinessVerdict::Missing => "unknown",
    }
}

fn evidence_kind_label(kind: EvidenceKind) -> &'static str {
    match kind {
        EvidenceKind::Verification => "verify",
        EvidenceKind::Conflict => "conflict",
        EvidenceKind::Tests => "tests",
        EvidenceKind::Format => "fmt",
        EvidenceKind::Lint => "lint",
        EvidenceKind::Review => "review",
        EvidenceKind::Diff => "diff",
    }
}

fn evidence_state_label(state: EvidenceState) -> &'static str {
    match state {
        EvidenceState::Passed => "ok",
        EvidenceState::Failed => "fail",
        EvidenceState::Running => "run",
        EvidenceState::Missing => "?",
        EvidenceState::Stale => "stale",
    }
}

fn campaign_rows(snapshot: &FleetSnapshot) -> Vec<Row> {
    let Some(summary) = &snapshot.campaign else {
        return Vec::new();
    };
    let mut rows = vec![Row {
        line: format!(
            "{} tasks · {} ready · {} running · {} done · {} failed · {} blocked",
            summary.total,
            summary.ready,
            summary.running,
            summary.completed,
            summary.failed,
            summary.blocked
        ),
        accent: None,
        attention_id: None,
        session_id: None,
        checkpoint_id: None,
        campaign_id: None,
        campaign_task_id: None,
        trial_id: None,
        trial_contender_id: None,
    }];
    rows.extend(snapshot.campaign_tasks.iter().map(|task| Row {
        line: format!(
            "{:<9} {:<18} {}{}",
            campaign_task_state_label(task.state),
            truncate(&task.campaign_label, 18),
            task.title,
            if task.blocked_by_failure {
                " · blocked by failed dependency"
            } else {
                ""
            }
        ),
        accent: None,
        attention_id: None,
        session_id: task.links.session_id.clone(),
        checkpoint_id: None,
        campaign_id: Some(task.campaign_id.clone()),
        campaign_task_id: Some(task.id.clone()),
        trial_id: None,
        trial_contender_id: None,
    }));
    rows
}

fn campaign_task_state_label(state: crate::campaigns::CampaignTaskState) -> &'static str {
    use crate::campaigns::CampaignTaskState;
    match state {
        CampaignTaskState::Planned => "planned",
        CampaignTaskState::Ready => "ready",
        CampaignTaskState::Active => "active",
        CampaignTaskState::Waiting => "waiting",
        CampaignTaskState::Review => "review",
        CampaignTaskState::Done => "done",
        CampaignTaskState::Failed => "failed",
        CampaignTaskState::Cancelled => "cancelled",
    }
}

fn budget_rows(snapshot: &FleetSnapshot) -> Vec<Row> {
    let mut rows = Vec::new();
    let usage = snapshot.usage;
    match snapshot.limits {
        Some(limits) => rows.push(Row {
            line: format!("workers   {} of {} in use", usage.active, limits.max_active),
            accent: None,
            attention_id: None,
            session_id: None,
            checkpoint_id: None,
            campaign_id: None,
            campaign_task_id: None,
            trial_id: None,
            trial_contender_id: None,
        }),
        None => rows.push(Row {
            line: "workers   no cap configured".to_string(),
            accent: None,
            attention_id: None,
            session_id: None,
            checkpoint_id: None,
            campaign_id: None,
            campaign_task_id: None,
            trial_id: None,
            trial_contender_id: None,
        }),
    }
    // Reservations stay unknown until a usage adapter reports real numbers.
    rows.push(Row {
        line: "tokens    unknown (no usage adapter connected)".to_string(),
        accent: None,
        attention_id: None,
        session_id: None,
        checkpoint_id: None,
        campaign_id: None,
        campaign_task_id: None,
        trial_id: None,
        trial_contender_id: None,
    });
    rows.extend(snapshot.rejected.iter().map(|title| Row {
        line: format!("over cap  {title}"),
        accent: None,
        attention_id: None,
        session_id: None,
        checkpoint_id: None,
        campaign_id: None,
        campaign_task_id: None,
        trial_id: None,
        trial_contender_id: None,
    }));
    rows
}

fn trial_rows(snapshot: &FleetSnapshot) -> Vec<Row> {
    let mut rows = Vec::new();
    for trial in &snapshot.trials {
        rows.push(Row {
            line: format!(
                "{:<10} {:<32} {}/{} complete · {} failed{}",
                trial_state_label(trial.state),
                truncate(&trial.prompt, 32),
                trial.completed,
                trial.total,
                trial.failed,
                trial
                    .verification_recipe
                    .as_ref()
                    .map(|recipe| format!(" · verify {}", truncate(&recipe.command, 24)))
                    .unwrap_or_default(),
            ),
            accent: None,
            attention_id: None,
            session_id: trial.session_ids.first().cloned(),
            checkpoint_id: None,
            campaign_id: None,
            campaign_task_id: None,
            trial_id: Some(trial.id.clone()),
            trial_contender_id: None,
        });
        rows.extend(trial.contenders.iter().map(|contender| {
            let elapsed = contender
                .elapsed_ms
                .map(|elapsed| format!("{elapsed} ms"))
                .unwrap_or_else(|| "time unknown".to_string());
            let tokens = contender
                .reported_tokens
                .map(|tokens| format!("{tokens} tokens"))
                .unwrap_or_else(|| "tokens unknown".to_string());
            let error = contender
                .error
                .as_ref()
                .map(|error| format!(" · {}", truncate(error, 28)))
                .unwrap_or_default();
            let verification = contender
                .verification
                .as_ref()
                .map(|result| format!(" · verify {}", verification_state_label(result.state)))
                .unwrap_or_else(|| " · verify ?".to_string());
            let diff = contender.changed_files.map_or_else(
                || " · diff ?".to_string(),
                |files| {
                    format!(
                        " · {files} files +{}/-{}",
                        contender.insertions.unwrap_or(0),
                        contender.deletions.unwrap_or(0)
                    )
                },
            );
            let rank = trial
                .ranking
                .iter()
                .find(|rank| rank.contender_id == contender.id)
                .map(|rank| format!(" · #{}", rank.rank))
                .unwrap_or_default();
            let retained = if contender.retained { " · retained" } else { "" };
            let retries = if contender.prior_attempts > 0 {
                format!(" · {} retries", contender.prior_attempts)
            } else {
                String::new()
            };
            Row {
                line: format!(
                    "  {:<10} {:<16} {elapsed} · {tokens}{verification}{diff}{rank}{retained}{retries}{error}",
                    contender_state_label(contender.state),
                    truncate(&contender.tool, 16),
                ),
                accent: None,
                attention_id: None,
                session_id: contender.session_id.clone(),
                checkpoint_id: None,
                campaign_id: None,
                campaign_task_id: None,
                trial_id: Some(trial.id.clone()),
                trial_contender_id: Some(contender.id.clone()),
            }
        }));
    }
    rows
}

fn verification_state_label(state: crate::trials::VerificationState) -> &'static str {
    match state {
        crate::trials::VerificationState::Passed => "passed",
        crate::trials::VerificationState::Failed => "failed",
        crate::trials::VerificationState::TimedOut => "timeout",
        crate::trials::VerificationState::Error => "error",
    }
}

fn trial_state_label(state: TrialState) -> &'static str {
    match state {
        TrialState::Launching => "launching",
        TrialState::Running => "running",
        TrialState::Partial => "partial",
        TrialState::Completed => "complete",
        TrialState::Failed => "failed",
        TrialState::Cancelled => "cancelled",
    }
}

fn contender_state_label(state: ContenderState) -> &'static str {
    match state {
        ContenderState::Pending => "pending",
        ContenderState::Creating => "creating",
        ContenderState::Starting => "starting",
        ContenderState::Running => "running",
        ContenderState::Completed => "complete",
        ContenderState::Failed => "failed",
        ContenderState::Cancelled => "cancelled",
    }
}

fn checkpoint_rows(snapshot: &FleetSnapshot) -> Vec<Row> {
    snapshot
        .checkpoints
        .iter()
        .map(|checkpoint| {
            let mut details = checkpoint.detail.clone().into_iter().collect::<Vec<_>>();
            if !checkpoint.session_available {
                details.push("source session is not active".to_string());
            }
            let tail = if details.is_empty() {
                checkpoint.snapshot_ref.clone()
            } else {
                format!("{} · {}", details.join(" · "), checkpoint.snapshot_ref)
            };
            Row {
                line: format!(
                    "{:<10} {:<16} {:<14} {}",
                    checkpoint_state_label(checkpoint.state),
                    truncate(&checkpoint.title, 16),
                    truncate(&checkpoint.label, 14),
                    tail
                ),
                accent: None,
                attention_id: None,
                session_id: checkpoint
                    .session_available
                    .then(|| checkpoint.session_id.clone()),
                checkpoint_id: (!matches!(checkpoint.state, CheckpointState::Materializing))
                    .then(|| checkpoint.checkpoint_id.clone()),
                campaign_id: None,
                campaign_task_id: None,
                trial_id: None,
                trial_contender_id: None,
            }
        })
        .collect()
}

fn checkpoint_state_label(state: CheckpointState) -> &'static str {
    match state {
        CheckpointState::Materializing => "capturing",
        CheckpointState::Complete => "complete",
        CheckpointState::Incomplete => "incomplete",
    }
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    let mut out: String = value.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::fleet_state::{AttentionRow, CheckpointRow, ConflictRow};

    fn snapshot() -> FleetSnapshot {
        FleetSnapshot {
            sessions: 2,
            attention: vec![
                AttentionRow {
                    item_id: format!("sha256:{}", "a".repeat(64)),
                    session_id: "one".into(),
                    title: "carthage".into(),
                    reason: AttentionReason::Failed,
                    summary: "session reported an error".into(),
                    since_ms: 1,
                },
                AttentionRow {
                    item_id: format!("sha256:{}", "b".repeat(64)),
                    session_id: "two".into(),
                    title: "rome".into(),
                    reason: AttentionReason::Approval,
                    summary: "agent is waiting on you".into(),
                    since_ms: 2,
                },
            ],
            conflicts: vec![ConflictRow {
                repo: "repo".into(),
                path: "src/lib.rs".into(),
                session_ids: vec!["one".into(), "two".into()],
                sessions: vec!["carthage".into(), "rome".into()],
            }],
            ..FleetSnapshot::default()
        }
    }

    fn press(
        dialog: &mut FleetDialog,
        code: KeyCode,
        snapshot: &FleetSnapshot,
    ) -> FleetDialogResult {
        dialog.handle_key(KeyEvent::new(code, KeyModifiers::NONE), snapshot)
    }

    #[test]
    fn tab_cycles_panels_in_both_directions() {
        let snapshot = snapshot();
        let mut dialog = FleetDialog::new();
        let cases = [
            (KeyCode::Tab, FleetTab::Conflicts),
            (KeyCode::Tab, FleetTab::Readiness),
            (KeyCode::BackTab, FleetTab::Conflicts),
            (KeyCode::Left, FleetTab::Attention),
            // Wraps rather than sticking at either end.
            (KeyCode::Left, FleetTab::Checkpoints),
        ];
        for (code, expected) in cases {
            press(&mut dialog, code, &snapshot);
            assert_eq!(dialog.tab, expected, "{code:?}");
        }
    }

    #[test]
    fn enter_jumps_to_the_owning_session() {
        let snapshot = snapshot();
        let mut dialog = FleetDialog::new();
        press(&mut dialog, KeyCode::Down, &snapshot);
        match press(&mut dialog, KeyCode::Enter, &snapshot) {
            FleetDialogResult::Jump(id) => assert_eq!(id, "two"),
            _ => panic!("Enter on an attention row must jump"),
        }
        // A conflict row opens the first stable owner; `s` uses the same
        // target for the ordinary message composer.
        press(&mut dialog, KeyCode::Tab, &snapshot);
        assert!(matches!(
            press(&mut dialog, KeyCode::Enter, &snapshot),
            FleetDialogResult::Jump(id) if id == "one"
        ));
        assert!(matches!(
            press(&mut dialog, KeyCode::Char('s'), &snapshot),
            FleetDialogResult::CoordinateConflict { session_id } if session_id == "one"
        ));
    }

    #[test]
    fn attention_acknowledges_only_the_selected_item() {
        let snapshot = snapshot();
        let expected = snapshot.attention[0].item_id.clone();
        let mut dialog = FleetDialog::new();
        assert!(matches!(
            press(&mut dialog, KeyCode::Char('a'), &snapshot),
            FleetDialogResult::AcknowledgeAttention { item_id } if item_id == expected
        ));
    }

    #[test]
    fn conflict_refresh_is_panel_scoped() {
        let snapshot = snapshot();
        let mut dialog = FleetDialog::new();
        assert!(matches!(
            press(&mut dialog, KeyCode::Char('r'), &snapshot),
            FleetDialogResult::Continue
        ));
        press(&mut dialog, KeyCode::Tab, &snapshot);
        assert!(matches!(
            press(&mut dialog, KeyCode::Char('r'), &snapshot),
            FleetDialogResult::RefreshConflicts
        ));
    }

    #[test]
    fn the_filter_owns_printable_keys_and_esc_unwinds_one_step_at_a_time() {
        let snapshot = snapshot();
        let mut dialog = FleetDialog::new();
        press(&mut dialog, KeyCode::Char('/'), &snapshot);
        // `j` and `k` are navigation outside the filter and text inside it.
        for c in ['r', 'o', 'm', 'k'] {
            press(&mut dialog, KeyCode::Char(c), &snapshot);
        }
        assert_eq!(dialog.filter, "romk");
        assert_eq!(dialog.rows(&snapshot).len(), 0, "no row matches");
        press(&mut dialog, KeyCode::Backspace, &snapshot);
        assert_eq!(dialog.rows(&snapshot).len(), 1, "rome only");

        press(&mut dialog, KeyCode::Enter, &snapshot);
        assert!(!dialog.filtering, "Enter commits the filter");
        // First Esc clears the filter, second closes the panel.
        assert!(matches!(
            press(&mut dialog, KeyCode::Esc, &snapshot),
            FleetDialogResult::Continue
        ));
        assert!(dialog.filter.is_empty());
        assert!(matches!(
            press(&mut dialog, KeyCode::Esc, &snapshot),
            FleetDialogResult::Cancel
        ));
    }

    #[test]
    fn empty_panels_explain_why_they_are_empty() {
        let snapshot = FleetSnapshot::default();
        let cases = [
            (FleetTab::Conflicts, "No repository scan has run yet."),
            (FleetTab::Trials, "Trial history has not been loaded."),
        ];
        for (tab, expected) in cases {
            let message = empty_message(tab, &snapshot, "", false);
            assert!(message.starts_with(expected), "{tab:?}: {message}");
        }
        assert_eq!(
            empty_message(FleetTab::Attention, &snapshot, "zzz", false),
            "Nothing in Attention matches \"zzz\"."
        );
    }

    #[test]
    fn campaign_editor_creates_dependencies_and_rows_emit_state_actions() {
        let mut snapshot = snapshot();
        snapshot.campaign = Some(crate::fleet::CampaignSummary {
            total: 1,
            ready: 1,
            ..crate::fleet::CampaignSummary::default()
        });
        snapshot.campaign_load = CampaignLoadState::Ready {
            loaded: 1,
            errors: Vec::new(),
        };
        snapshot
            .campaign_tasks
            .push(crate::campaigns::CampaignTaskSummary {
                campaign_id: "campaign-1".into(),
                campaign_label: "Release".into(),
                id: "task-1".into(),
                title: "Foundation".into(),
                state: crate::campaigns::CampaignTaskState::Ready,
                ready: true,
                blocked_by_failure: false,
                dependency_ids: Vec::new(),
                links: crate::campaigns::CampaignTaskLinks {
                    session_id: Some("one".into()),
                    ..crate::campaigns::CampaignTaskLinks::default()
                },
                updated_at_ms: 1,
            });
        let target = CheckpointTarget {
            session_id: "one".into(),
            title: "carthage".into(),
        };
        let mut dialog = FleetDialog::new().with_checkpoint_target(Some(target));
        for _ in 0..3 {
            press(&mut dialog, KeyCode::Tab, &snapshot);
        }
        press(&mut dialog, KeyCode::Char('n'), &snapshot);
        dialog.handle_paste("Ship docs");
        press(&mut dialog, KeyCode::Enter, &snapshot);
        press(&mut dialog, KeyCode::Char(' '), &snapshot);
        match press(&mut dialog, KeyCode::Enter, &snapshot) {
            FleetDialogResult::CreateCampaignTask {
                campaign_id,
                title,
                dependency_ids,
                session_id,
            } => {
                assert_eq!(campaign_id.as_deref(), Some("campaign-1"));
                assert_eq!(title, "Ship docs");
                assert_eq!(dependency_ids, ["task-1"]);
                assert_eq!(session_id.as_deref(), Some("one"));
            }
            _ => panic!("campaign dependency picker should create the task"),
        }

        press(&mut dialog, KeyCode::Down, &snapshot);
        assert!(matches!(
            press(&mut dialog, KeyCode::Char('a'), &snapshot),
            FleetDialogResult::UpdateCampaignTask {
                campaign_id,
                task_id,
                state: crate::campaigns::CampaignTaskState::Active,
            } if campaign_id == "campaign-1" && task_id == "task-1"
        ));
        for (key, expected) in [
            (
                KeyCode::Char('f'),
                crate::campaigns::CampaignTaskState::Failed,
            ),
            (
                KeyCode::Char('x'),
                crate::campaigns::CampaignTaskState::Cancelled,
            ),
        ] {
            assert!(matches!(
                press(&mut dialog, key, &snapshot),
                FleetDialogResult::UpdateCampaignTask {
                    campaign_id,
                    task_id,
                    state,
                } if campaign_id == "campaign-1" && task_id == "task-1" && state == expected
            ));
        }
    }

    #[test]
    fn trial_editors_own_vim_keys_and_require_two_contenders() {
        let mut snapshot = snapshot();
        snapshot.checkpoints.push(CheckpointRow {
            checkpoint_id: "checkpoint-1".into(),
            session_id: "one".into(),
            session_available: true,
            title: "carthage".into(),
            label: "baseline".into(),
            snapshot_ref: "123456789abc".into(),
            created_at_ms: 10,
            state: CheckpointState::Complete,
            detail: None,
        });
        let mut dialog =
            FleetDialog::new().with_trial_tools(vec!["claude".to_string(), "codex".to_string()]);
        press(&mut dialog, KeyCode::Left, &snapshot);
        press(&mut dialog, KeyCode::Char('t'), &snapshot);
        press(&mut dialog, KeyCode::Char('j'), &snapshot);
        press(&mut dialog, KeyCode::Char('k'), &snapshot);
        dialog.handle_paste(" pasted");
        assert_eq!(
            dialog
                .trial_prompt
                .as_ref()
                .map(|editor| editor.prompt.as_str()),
            Some("jk pasted")
        );

        press(&mut dialog, KeyCode::Enter, &snapshot);
        dialog.handle_paste("cargo test");
        press(&mut dialog, KeyCode::Enter, &snapshot);
        press(&mut dialog, KeyCode::Char(' '), &snapshot);
        assert!(matches!(
            press(&mut dialog, KeyCode::Enter, &snapshot),
            FleetDialogResult::Continue
        ));
        press(&mut dialog, KeyCode::Char(' '), &snapshot);
        match press(&mut dialog, KeyCode::Enter, &snapshot) {
            FleetDialogResult::CreateTrial {
                prompt,
                verification_command,
                tools,
                ..
            } => {
                assert_eq!(prompt, "jk pasted");
                assert_eq!(verification_command, "cargo test");
                assert_eq!(tools, ["claude", "codex"]);
            }
            _ => panic!("two selected contenders should launch the trial"),
        }
    }

    #[test]
    fn one_available_agent_can_run_two_fresh_contenders() {
        let mut snapshot = snapshot();
        snapshot.checkpoints.push(CheckpointRow {
            checkpoint_id: "checkpoint-1".into(),
            session_id: "one".into(),
            session_available: true,
            title: "carthage".into(),
            label: "baseline".into(),
            snapshot_ref: "123456789abc".into(),
            created_at_ms: 10,
            state: CheckpointState::Complete,
            detail: None,
        });
        let mut dialog = FleetDialog::new().with_trial_tools(vec!["codex".to_string()]);
        press(&mut dialog, KeyCode::Left, &snapshot);
        press(&mut dialog, KeyCode::Char('t'), &snapshot);
        press(&mut dialog, KeyCode::Char('x'), &snapshot);
        press(&mut dialog, KeyCode::Enter, &snapshot);
        dialog.handle_paste("true");
        press(&mut dialog, KeyCode::Enter, &snapshot);
        match press(&mut dialog, KeyCode::Enter, &snapshot) {
            FleetDialogResult::CreateTrial { tools, .. } => {
                assert_eq!(tools, ["codex", "codex"]);
            }
            _ => panic!("the duplicated A/B choices should be launchable"),
        }
    }

    #[test]
    fn trial_rows_emit_only_eligible_retry_cancel_and_retain_actions() {
        let mut request = crate::trials::TrialCreateRequest::new(
            uuid::Uuid::new_v4().to_string(),
            "compare implementations",
            ["claude".to_string(), "codex".to_string()],
            "default",
        )
        .unwrap();
        let trial_id = request.manifest.id.clone();
        let contender_id = request.manifest.contenders[0].spec.id.clone();
        request.manifest.state = TrialState::Running;
        request.manifest.contenders[0].state = ContenderState::Failed;
        request.manifest.contenders[0].session_id = Some("one".to_string());
        request.manifest.contenders[0].error = Some("verification failed".to_string());
        request.manifest.contenders[1].state = ContenderState::Running;
        request.manifest.contenders[1].session_id = Some("two".to_string());

        let mut snapshot = snapshot();
        snapshot.trials = vec![request.manifest.summary()];
        let mut dialog = FleetDialog::new();
        for _ in 0..5 {
            press(&mut dialog, KeyCode::Tab, &snapshot);
        }
        press(&mut dialog, KeyCode::Down, &snapshot);
        assert!(matches!(
            press(&mut dialog, KeyCode::Char('y'), &snapshot),
            FleetDialogResult::RetryTrialContender {
                trial_id: emitted_trial,
                contender_id: emitted_contender,
            } if emitted_trial == trial_id && emitted_contender == contender_id
        ));
        assert!(matches!(
            press(&mut dialog, KeyCode::Char('x'), &snapshot),
            FleetDialogResult::CancelTrial { trial_id: emitted } if emitted == trial_id
        ));
        assert!(matches!(
            press(&mut dialog, KeyCode::Char('w'), &snapshot),
            FleetDialogResult::Continue
        ));

        request.manifest.state = TrialState::Completed;
        request.manifest.contenders[0].state = ContenderState::Completed;
        request.manifest.contenders[0].error = None;
        snapshot.trials = vec![request.manifest.summary()];
        assert!(matches!(
            press(&mut dialog, KeyCode::Char('w'), &snapshot),
            FleetDialogResult::RetainTrialWinner {
                trial_id: emitted_trial,
                contender_id: emitted_contender,
            } if emitted_trial == trial_id && emitted_contender == contender_id
        ));
        assert!(matches!(
            press(&mut dialog, KeyCode::Char('x'), &snapshot),
            FleetDialogResult::Continue
        ));
    }

    #[test]
    fn checkpoint_panel_loads_once_and_label_input_owns_printable_keys() {
        let mut snapshot = snapshot();
        snapshot.checkpoints.push(CheckpointRow {
            checkpoint_id: "checkpoint-1".into(),
            session_id: "one".into(),
            session_available: true,
            title: "carthage".into(),
            label: "before trial".into(),
            snapshot_ref: "123456789abc".into(),
            created_at_ms: 10,
            state: CheckpointState::Complete,
            detail: None,
        });
        let target = CheckpointTarget {
            session_id: "one".into(),
            title: "carthage".into(),
        };
        let mut dialog = FleetDialog::new().with_checkpoint_target(Some(target));

        assert!(matches!(
            press(&mut dialog, KeyCode::Left, &snapshot),
            FleetDialogResult::LoadCheckpoints
        ));
        assert!(matches!(
            press(&mut dialog, KeyCode::Char('f'), &snapshot),
            FleetDialogResult::ForkCheckpoint {
                checkpoint_id,
                session_id,
            } if checkpoint_id == "checkpoint-1" && session_id == "one"
        ));
        assert!(matches!(
            press(&mut dialog, KeyCode::Char('i'), &snapshot),
            FleetDialogResult::InspectCheckpoint { checkpoint_id }
                if checkpoint_id == "checkpoint-1"
        ));
        assert!(matches!(
            press(&mut dialog, KeyCode::Char('e'), &snapshot),
            FleetDialogResult::ExportCheckpoint { checkpoint_id }
                if checkpoint_id == "checkpoint-1"
        ));
        assert!(matches!(
            press(&mut dialog, KeyCode::Char('d'), &snapshot),
            FleetDialogResult::DeleteCheckpoint { checkpoint_id }
                if checkpoint_id == "checkpoint-1"
        ));
        assert!(matches!(
            press(&mut dialog, KeyCode::Char('r'), &snapshot),
            FleetDialogResult::RestoreCheckpoint {
                checkpoint_id,
                session_id,
            } if checkpoint_id == "checkpoint-1" && session_id == "one"
        ));
        press(&mut dialog, KeyCode::Char('c'), &snapshot);
        for c in ['j', 'k', ' ', 'v', '1'] {
            press(&mut dialog, KeyCode::Char(c), &snapshot);
        }
        match press(&mut dialog, KeyCode::Enter, &snapshot) {
            FleetDialogResult::CaptureCheckpoint { session_id, label } => {
                assert_eq!(session_id, "one");
                assert_eq!(label, "jk v1");
            }
            _ => panic!("Enter in the label editor must request capture"),
        }

        // Re-entering the same panel does not request another disk load.
        press(&mut dialog, KeyCode::Right, &snapshot);
        assert!(matches!(
            press(&mut dialog, KeyCode::Left, &snapshot),
            FleetDialogResult::Continue
        ));
        // Existing rows retain the ordinary Enter-to-session behavior.
        assert!(matches!(
            press(&mut dialog, KeyCode::Enter, &snapshot),
            FleetDialogResult::Jump(id) if id == "one"
        ));
    }

    #[test]
    fn checkpoint_fork_requires_a_finished_row_with_an_active_source() {
        let mut snapshot = snapshot();
        snapshot.checkpoints = vec![
            CheckpointRow {
                checkpoint_id: "capturing".into(),
                session_id: "one".into(),
                session_available: true,
                title: "carthage".into(),
                label: "still writing".into(),
                snapshot_ref: "capturing".into(),
                created_at_ms: 20,
                state: CheckpointState::Materializing,
                detail: None,
            },
            CheckpointRow {
                checkpoint_id: "orphan".into(),
                session_id: "gone".into(),
                session_available: false,
                title: "rome".into(),
                label: "finished".into(),
                snapshot_ref: "123456789abc".into(),
                created_at_ms: 10,
                state: CheckpointState::Complete,
                detail: None,
            },
        ];
        let mut dialog = FleetDialog::new();
        press(&mut dialog, KeyCode::Left, &snapshot);

        assert!(matches!(
            press(&mut dialog, KeyCode::Char('f'), &snapshot),
            FleetDialogResult::Continue
        ));
        press(&mut dialog, KeyCode::Down, &snapshot);
        assert!(matches!(
            press(&mut dialog, KeyCode::Char('f'), &snapshot),
            FleetDialogResult::Continue
        ));
    }

    #[test]
    fn escape_cancels_only_the_checkpoint_label_editor() {
        let snapshot = snapshot();
        let target = CheckpointTarget {
            session_id: "one".into(),
            title: "carthage".into(),
        };
        let mut dialog = FleetDialog::new().with_checkpoint_target(Some(target));
        press(&mut dialog, KeyCode::Left, &snapshot);
        press(&mut dialog, KeyCode::Char('c'), &snapshot);

        assert!(matches!(
            press(&mut dialog, KeyCode::Esc, &snapshot),
            FleetDialogResult::Continue
        ));
        assert!(matches!(
            press(&mut dialog, KeyCode::Esc, &snapshot),
            FleetDialogResult::Cancel
        ));
    }
}
