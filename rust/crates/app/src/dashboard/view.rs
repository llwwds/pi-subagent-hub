//! View model for the SSR panel.
//!
//! Every value here is already display-ready: the Askama template performs no
//! derivation beyond iteration and conditional sections.

use pihub_contracts::{
    Carrier, CarrierCapabilities, CarrierDiagnostic, FrozenPromptConfig, PromptMode, ProviderApp,
    ProviderSnapshot, RuntimeRequestKind, SkillManagerAction, SkillSummary, StoredEvent,
};
use askama::Template;
use serde_json::Value;

use super::{
    PanelInputs, app_label, carrier_label, event_summary, lifecycle_label, model_label, provider_ready,
    providers_for, safe_base_url, short, skill_policy_label, status_meta, time_label,
};

/// Panel query state preserved across links and forms.
#[derive(Debug, Clone, Default)]
pub struct QueryState {
    pub view: String,
    pub agent: Option<String>,
    pub q: String,
    pub carrier: String,
    pub status: String,
    pub verbose: bool,
}

#[derive(Debug, Clone)]
pub struct Choice {
    pub value: String,
    pub label: String,
    pub selected: bool,
    pub disabled: bool,
}

#[derive(Debug, Clone)]
pub struct Fact {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct Activity {
    pub id: String,
    pub tone: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct AgentCard {
    pub id: String,
    pub name: String,
    pub short_id: String,
    pub carrier_label: String,
    pub status_label: String,
    pub status_tone: String,
    pub model: String,
    pub policy: String,
    pub recent: String,
    pub recent_title: String,
    pub created: String,
    pub selected: bool,
}

#[derive(Debug, Clone)]
pub struct Chip {
    pub label: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct TimelineRow {
    pub label: String,
    pub summary: String,
    pub time: String,
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub time: String,
    pub kind: String,
    pub summary: String,
    pub raw: String,
}

#[derive(Debug, Clone)]
pub struct SkillCard {
    pub id: String,
    pub name: String,
    pub version_label: String,
    pub facts: Vec<Fact>,
}

#[derive(Debug, Clone, Default)]
pub struct ManagerView {
    pub status_text: String,
    pub tone: String,
    pub connected: bool,
    pub facts: Vec<Fact>,
    pub note: String,
    pub can_connect: bool,
    pub can_sync: bool,
    pub can_disconnect: bool,
}

#[derive(Debug, Clone)]
pub struct ProviderRow {
    pub id: String,
    pub name: String,
    pub badge: String,
    pub badge_class: String,
    pub facts: Vec<Fact>,
    pub models: Vec<String>,
    pub models_label: String,
    pub unconfigured: bool,
    pub action_label: String,
    pub action_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct ProviderAppBlock {
    pub app_id: String,
    pub app_label: String,
    pub note: String,
    pub count_label: String,
    pub rows: Vec<ProviderRow>,
    pub empty_text: String,
}

/// One `apiFormat` choice, scoped to the carrier that accepts it.
#[derive(Debug, Clone)]
pub struct ApiFormatOption {
    pub app: String,
    pub value: String,
    pub label: String,
    pub is_default: bool,
    pub hidden: bool,
}

#[derive(Debug, Clone)]
pub struct CarrierRow {
    pub id: String,
    pub label: String,
    pub available: bool,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct SkillOption {
    pub id: String,
    pub name: String,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct PrincipleRow {
    pub id: String,
    pub title: String,
    pub text: String,
    pub checked: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DetailView {
    pub id: String,
    pub name: String,
    pub carrier_label: String,
    pub status_label: String,
    pub status_tone: String,
    pub created: String,
    pub updated: String,
    pub carrier: String,
    pub model: String,
    pub thinking: String,
    pub process: String,
    pub cwd: String,
    pub skill_policy_label: String,
    pub skill_chips: Vec<Chip>,
    pub prompt_summary: String,
    pub prompt_title: String,
    pub prompt_chips: Vec<Chip>,
    pub prompt_text: String,
    pub prompt_details_visible: bool,
    pub last_error: String,
    pub has_error: bool,
    pub can_send: bool,
    pub can_abort: bool,
    pub can_stop: bool,
    pub can_restart: bool,
    pub placeholder: String,
    pub message_kinds: Vec<Choice>,
    pub timeline: Vec<TimelineRow>,
    pub timeline_count: usize,
    pub events: Vec<EventRow>,
}

#[derive(askama::Template)]
#[template(path = "panel.html")]
pub struct PanelView {
    pub token: String,
    pub version: String,
    pub view_agents: bool,
    pub view_skills: bool,
    pub view_providers: bool,
    pub refresh_href: String,
    pub skills_href: String,
    pub providers_href: String,
    pub agents_href: String,
    pub skill_count: usize,
    pub toast: String,
    pub toast_tone: String,
    pub warnings: Vec<String>,
    pub last_sync: String,
    pub active_count: usize,
    pub managed_count: usize,
    pub running_count: usize,
    pub waiting_count: usize,
    pub faulted_count: usize,
    pub carrier_available: usize,
    pub activity: Vec<Activity>,
    pub activity_empty: bool,
    pub agent_total: usize,
    pub agents: Vec<AgentCard>,
    pub list_empty: String,
    pub filter_q: String,
    pub filter_carrier: String,
    pub filter_status: String,
    pub carrier_filter_options: Vec<Choice>,
    pub status_filter_options: Vec<Choice>,
    pub detail_present: bool,
    pub detail: DetailView,
    pub verbose: bool,
    pub verbose_href: String,
    pub terse_href: String,
    pub create_open: bool,
    pub provider_open: bool,
    pub create_carriers: Vec<Choice>,
    pub create_carrier_rows: Vec<CarrierRow>,
    pub create_providers: Vec<Choice>,
    pub create_models: Vec<Choice>,
    pub create_skills: Vec<SkillOption>,
    pub principles: Vec<PrincipleRow>,
    pub prompt_note: String,
    pub thinking_options: Vec<Choice>,
    pub skills: Vec<SkillCard>,
    pub manager: ManagerView,
    pub provider_status_text: String,
    pub provider_status_ready: bool,
    pub provider_blocks: Vec<ProviderAppBlock>,
    pub api_formats: Vec<ApiFormatOption>,
    pub claude_auth_modes: Vec<Choice>,
}

const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

impl PanelView {
    pub fn build(inputs: PanelInputs<'_>) -> Self {
        let PanelInputs {
            token,
            query,
            snapshot,
            carriers,
            skills,
            prompts,
            manager,
            providers,
            agents,
            detail_events,
            frozen,
            create_open,
            provider_open,
            toast,
            error,
            warnings,
        } = inputs;

        let view = match query.view.as_str() {
            "skills" => "skills",
            "providers" => "providers",
            _ => "agents",
        };
        let active_count = snapshot.as_ref().map(|value| value.active_count).unwrap_or(0);
        let managed_count = snapshot
            .as_ref()
            .map(|value| value.managed_count)
            .unwrap_or(agents.len());
        let event_cursor = snapshot.as_ref().map(|value| value.event_cursor).unwrap_or(0);

        let mut running_count = 0usize;
        let mut waiting_count = 0usize;
        let mut faulted_count = 0usize;
        for agent in &agents {
            match agent.agent.status.as_str() {
                "running" => running_count += 1,
                "waiting_input" => waiting_count += 1,
                "crashed" | "failed" => faulted_count += 1,
                _ => {}
            }
        }
        let carrier_available = carriers.iter().filter(|carrier| carrier.available).count();

        let activity: Vec<Activity> = agents
            .iter()
            .take(48)
            .map(|agent| {
                let (label, tone) = status_meta(agent.agent.status.as_str());
                Activity {
                    id: agent.agent.id.clone(),
                    tone: tone.to_string(),
                    title: format!("{} · {}", agent.agent.name, label),
                }
            })
            .collect();

        let selected = query
            .agent
            .as_ref()
            .and_then(|id| agents.iter().find(|agent| &agent.agent.id == id));

        let detail = match selected {
            Some(agent) => build_detail(agent, &carriers, &detail_events, frozen.as_ref(), query.verbose),
            None => DetailView::default(),
        };

        let needle = query.q.to_lowercase();
        let filtered: Vec<&AgentViewV2Alias> = agents
            .iter()
            .filter(|agent| {
                let haystack = format!("{} {}", agent.agent.name, agent.agent.id).to_lowercase();
                let matches_query = needle.is_empty() || haystack.contains(&needle);
                let matches_carrier =
                    query.carrier.is_empty() || agent.carrier.as_str() == query.carrier;
                let matches_status =
                    query.status.is_empty() || agent.agent.status.as_str() == query.status;
                matches_query && matches_carrier && matches_status
            })
            .collect();

        let list_empty = if agents.is_empty() {
            "尚无 Agent。使用右上角按钮创建第一个实例。".to_string()
        } else {
            "没有符合筛选条件的 Agent。".to_string()
        };

        let mut carrier_ids: Vec<String> = carriers
            .iter()
            .map(|carrier| carrier.id.as_str().to_string())
            .collect();
        for agent in &agents {
            let id = agent.carrier.as_str().to_string();
            if !carrier_ids.contains(&id) {
                carrier_ids.push(id);
            }
        }
        let carrier_filter_options = std::iter::once(Choice {
            value: String::new(),
            label: "全部载体".to_string(),
            selected: query.carrier.is_empty(),
            disabled: false,
        })
        .chain(carrier_ids.iter().map(|id| Choice {
            value: id.clone(),
            label: carrier_label(&carriers, id),
            selected: query.carrier == *id,
            disabled: false,
        }))
        .collect();

        let status_filter_options = std::iter::once(Choice {
            value: String::new(),
            label: "全部状态".to_string(),
            selected: query.status.is_empty(),
            disabled: false,
        })
        .chain(
            [
                ("running", "执行中"),
                ("idle", "待命"),
                ("waiting_input", "等待输入"),
                ("starting", "启动中"),
                ("provisioning", "准备中"),
                ("stopping", "停止中"),
                ("stopped", "已停止"),
                ("crashed", "崩溃"),
                ("failed", "失败"),
            ]
            .iter()
            .map(|(value, label)| Choice {
                value: value.to_string(),
                label: label.to_string(),
                selected: query.status == *value,
                disabled: false,
            }),
        )
        .collect();

        let create_carriers: Vec<Choice> = Carrier::ALL
            .iter()
            .map(|carrier| {
                let id = carrier.as_str();
                let diagnostic = carriers.iter().find(|row| row.id.as_str() == id);
                let available = diagnostic.map(|row| row.available).unwrap_or(false);
                Choice {
                    value: id.to_string(),
                    label: if available {
                        carrier_label(&carriers, id)
                    } else {
                        format!("{} · 不可用", carrier_label(&carriers, id))
                    },
                    selected: false,
                    disabled: !available,
                }
            })
            .collect();

        let create_carrier_rows: Vec<CarrierRow> = Carrier::ALL
            .iter()
            .map(|carrier| {
                let id = carrier.as_str().to_string();
                let diagnostic = carriers.iter().find(|row| row.id.as_str() == id);
                let available = diagnostic.map(|row| row.available).unwrap_or(false);
                CarrierRow {
                    label: carrier_label(&carriers, &id),
                    detail: if available {
                        "可用".to_string()
                    } else {
                        format!(
                            "不可用 · {}",
                            diagnostic
                                .and_then(|row| row.reason.clone())
                                .unwrap_or_else(|| "载体尚未注册".to_string())
                        )
                    },
                    available,
                    id,
                }
            })
            .collect();

        let mut create_providers: Vec<Choice> = vec![Choice {
            value: String::new(),
            label: "使用载体已有配置".to_string(),
            selected: true,
            disabled: false,
        }];
        let mut create_models: Vec<Choice> = vec![Choice {
            value: String::new(),
            label: "使用供应商默认模型".to_string(),
            selected: true,
            disabled: false,
        }];
        for app in ProviderApp::ALL {
            for provider in providers_for(providers.as_ref(), app).iter().filter(|provider| {
                provider_ready(provider)
            }) {
                create_providers.push(Choice {
                    value: provider.id.clone(),
                    label: format!(
                        "{} · {}{}",
                        app_label(app),
                        provider.name,
                        if provider.is_current.unwrap_or(false) {
                            " · 当前默认"
                        } else {
                            ""
                        }
                    ),
                    selected: false,
                    disabled: false,
                });
                for model in &provider.model_list {
                    create_models.push(Choice {
                        value: model.clone(),
                        label: format!("{} · {} · {}", app_label(app), provider.name, model),
                        selected: false,
                        disabled: false,
                    });
                }
            }
        }

        let create_skills: Vec<SkillOption> = skills
            .iter()
            .map(|skill| SkillOption {
                id: skill.id.clone(),
                name: skill.name.clone(),
                detail: format!("{} · {}", skill.id, short(&skill.current_sha256, 18)),
            })
            .collect();

        let principles: Vec<PrincipleRow> = prompts
            .principles
            .iter()
            .map(|principle| PrincipleRow {
                id: principle.id.clone(),
                title: principle.title.clone(),
                text: principle.text.clone(),
                checked: prompts.default_principle_ids.contains(&principle.id),
            })
            .collect();

        let prompt_note = "Pi 与 Claude 追加到 system prompt 末尾，Codex 追加到 developer 指令层；只追加，不替换载体自带约束。不支持追加通道的载体在显式请求时会被服务端拒绝。".to_string();

        let skills_cards: Vec<SkillCard> = skills
            .iter()
            .map(|skill: &SkillSummary| SkillCard {
                id: skill.id.clone(),
                name: skill.name.clone(),
                version_label: format!("{} 快照版本", skill.versions.len().max(1)),
                facts: vec![
                    Fact {
                        label: "来源".to_string(),
                        value: skill.source.clone(),
                    },
                    Fact {
                        label: "SHA-256".to_string(),
                        value: skill.current_sha256.clone(),
                    },
                    Fact {
                        label: "导入".to_string(),
                        value: time_label(&skill.imported_at),
                    },
                ],
            })
            .collect();

        let manager = build_manager(manager.as_ref(), &warnings);
        let provider_blocks = build_provider_blocks(providers.as_ref());
        let provider_status_text = match providers.as_ref() {
            Some(snapshot) if snapshot.switch.available => format!(
                "CC Switch CLI 分支已就绪{} · 应用私有配置",
                snapshot
                    .switch
                    .version
                    .as_ref()
                    .map(|version| format!(" · {version}"))
                    .unwrap_or_default()
            ),
            Some(snapshot) => format!(
                "CC Switch 暂不可用 · {}",
                snapshot
                    .switch
                    .reason
                    .clone()
                    .unwrap_or_else(|| "请检查应用内的 CC Switch 安装与配置".to_string())
            ),
            None => "CC Switch 状态不可用 · 供应商列表读取失败".to_string(),
        };
        let provider_status_ready = providers
            .as_ref()
            .map(|snapshot| snapshot.switch.available)
            .unwrap_or(false);

        let base = base_query(view, query.agent.as_deref(), &query);
        let refresh_href = base.clone();
        let verbose_href = format!(
            "{}{}verbose=1",
            base,
            if base.contains('?') { "&" } else { "?" }
        );
        let terse_href = base.clone();
        let skills_href = base_query("skills", None, &query);
        let providers_href = base_query("providers", None, &query);
        let agents_href = base_query("agents", None, &query);

        let thinking_options: Vec<Choice> = std::iter::once(Choice {
            value: String::new(),
            label: "使用载体默认值".to_string(),
            selected: true,
            disabled: false,
        })
        .chain(THINKING_LEVELS.iter().map(|level| Choice {
            value: level.to_string(),
            label: level.to_string(),
            selected: false,
            disabled: false,
        }))
        .collect();

        let api_formats = build_api_formats();
        let claude_auth_modes = build_claude_auth_modes();

        let toast_tone = if !error.is_empty() {
            " error"
        } else {
            ""
        };
        let toast_text = if !error.is_empty() {
            error
        } else {
            toast
        };

        let agent_cards: Vec<AgentCard> = filtered
            .iter()
            .map(|agent| {
                let (status_label, status_tone) = status_meta(agent.agent.status.as_str());
                let recent_event = detail_events
                    .iter()
                    .rev()
                    .find(|event| event.agent_id.as_deref() == Some(agent.agent.id.as_str()));
                let recent = match (&agent.agent.last_error, recent_event) {
                    (Some(error), _) => format!("错误 · {}", short(error, 62)),
                    (None, Some(event)) => format!(
                        "{} · {}",
                        event.event_type,
                        short(&event_summary(event), 52)
                    ),
                    (None, None) => "暂无最近事件".to_string(),
                };
                AgentCard {
                    id: agent.agent.id.clone(),
                    name: agent.agent.name.clone(),
                    short_id: short(&agent.agent.id, 36),
                    carrier_label: carrier_label(&carriers, agent.carrier.as_str()),
                    status_label: status_label.to_string(),
                    status_tone: status_tone.to_string(),
                    model: short(&model_label(&agent.agent), 30),
                    policy: skill_policy_label(agent),
                    recent_title: agent
                        .agent
                        .last_error
                        .clone()
                        .or_else(|| recent_event.map(event_summary))
                        .unwrap_or_default(),
                    recent,
                    created: time_label(&agent.agent.created_at),
                    selected: query.agent.as_deref() == Some(agent.agent.id.as_str()),
                }
            })
            .collect();

        Self {
            token: token.to_string(),
            version: pihub_contracts::APP_VERSION.to_string(),
            view_agents: view == "agents",
            view_skills: view == "skills",
            view_providers: view == "providers",
            refresh_href,
            skills_href,
            providers_href,
            agents_href,
            skill_count: skills_cards.len(),
            toast: toast_text,
            toast_tone: toast_tone.to_string(),
            warnings,
            last_sync: chrono::Local::now().format("%H:%M:%S").to_string(),
            active_count,
            managed_count,
            running_count,
            waiting_count,
            faulted_count,
            carrier_available,
            activity,
            activity_empty: agents.is_empty(),
            agent_total: agents.len(),
            agents: agent_cards,
            list_empty,
            filter_q: query.q.clone(),
            filter_carrier: query.carrier.clone(),
            filter_status: query.status.clone(),
            carrier_filter_options,
            status_filter_options,
            detail_present: selected.is_some(),
            detail,
            verbose: query.verbose,
            verbose_href,
            terse_href,
            create_open,
            provider_open,
            create_carriers,
            create_carrier_rows,
            create_providers,
            create_models,
            create_skills,
            principles,
            prompt_note,
            thinking_options,
            skills: skills_cards,
            manager,
            provider_status_text,
            provider_status_ready,
            provider_blocks,
            api_formats,
            claude_auth_modes,
        }
        .with_cursor(event_cursor)
    }

    fn with_cursor(mut self, _cursor: i64) -> Self {
        // Kept for symmetry with the JSON API; the panel does not paginate.
        self.warnings.shrink_to_fit();
        self
    }

    pub fn render_markup(&self) -> pihub_contracts::HubResult<String> {
        self.render().map_err(|error| {
            pihub_contracts::HubError::new("panel_render_failed", error.to_string())
        })
    }
}

type AgentViewV2Alias = pihub_contracts::AgentViewV2;

fn base_query(view: &str, agent: Option<&str>, query: &QueryState) -> String {
    let mut url = format!("/?view={view}");
    if let Some(agent) = agent {
        url.push_str(&format!("&agent={}", crate::forms::percent_encode(agent)));
    }
    if !query.q.is_empty() {
        url.push_str(&format!("&q={}", crate::forms::percent_encode(&query.q)));
    }
    if !query.carrier.is_empty() {
        url.push_str(&format!("&carrier={}", crate::forms::percent_encode(&query.carrier)));
    }
    if !query.status.is_empty() {
        url.push_str(&format!("&status={}", crate::forms::percent_encode(&query.status)));
    }
    url
}

fn build_detail(
    agent: &AgentViewV2Alias,
    carriers: &[CarrierDiagnostic],
    events: &[StoredEvent],
    frozen: Option<&FrozenPromptConfig>,
    verbose: bool,
) -> DetailView {
    let (status_label, status_tone) = status_meta(agent.agent.status.as_str());
    let carrier_id = agent.carrier.as_str();
    let capabilities: Option<&CarrierCapabilities> = carriers
        .iter()
        .find(|carrier| carrier.id.as_str() == carrier_id)
        .map(|carrier| &carrier.capabilities);
    let allowed_messages: Vec<RuntimeRequestKind> = capabilities
        .map(|capabilities| capabilities.messages.clone())
        .unwrap_or_else(|| {
            vec![
                RuntimeRequestKind::Prompt,
                RuntimeRequestKind::Steer,
                RuntimeRequestKind::FollowUp,
                RuntimeRequestKind::Abort,
            ]
        });

    let status = agent.agent.status.as_str();
    let can_abort = allowed_messages.contains(&RuntimeRequestKind::Abort)
        && matches!(status, "running" | "waiting_input");
    let can_stop = matches!(status, "running" | "idle" | "waiting_input" | "starting");
    let can_restart = !matches!(status, "provisioning" | "starting" | "stopping");
    let busy_cli = carrier_id != "pi" && status == "running";
    let can_send = !busy_cli && matches!(status, "running" | "idle" | "waiting_input");

    let mut message_kinds: Vec<Choice> = Vec::new();
    for (value, kind, label) in [
        ("prompt", RuntimeRequestKind::Prompt, "消息"),
        ("steer", RuntimeRequestKind::Steer, "调整当前任务"),
        ("follow-up", RuntimeRequestKind::FollowUp, "后续任务"),
    ] {
        if allowed_messages.contains(&kind) {
            message_kinds.push(Choice {
                value: value.to_string(),
                label: label.to_string(),
                selected: value == "prompt",
                disabled: false,
            });
        }
    }

    let skill_chips: Vec<Chip> = agent
        .skill_ids
        .iter()
        .map(|id| Chip {
            label: id.clone(),
            title: id.clone(),
        })
        .collect();

    let mut prompt_chips: Vec<Chip> = Vec::new();
    let (prompt_summary, prompt_text, prompt_details_visible, prompt_title) = match frozen {
        None => (
            "旧版 Agent · 未记录".to_string(),
            String::new(),
            false,
            String::new(),
        ),
        Some(frozen) if frozen.mode != PromptMode::Append => (
            "未追加（使用载体自带系统提示）".to_string(),
            String::new(),
            false,
            String::new(),
        ),
        Some(frozen) => {
            for id in &frozen.principle_ids {
                prompt_chips.push(Chip {
                    label: id.clone(),
                    title: id.clone(),
                });
            }
            if !frozen.custom_text.is_empty() {
                prompt_chips.push(Chip {
                    label: format!(
                        "自定义正文 {} 字符",
                        pihub_contracts::utf16_len(&frozen.custom_text)
                    ),
                    title: String::new(),
                });
            }
            let channel_label = match frozen.channel {
                Some(pihub_contracts::PromptChannel::Developer) => "Codex developer 指令层追加",
                _ => "system prompt 末尾追加",
            };
            let summary = format!(
                "{channel_label} · {} 字符 · {}",
                pihub_contracts::utf16_len(&frozen.prompt),
                short(&frozen.digest, 18)
            );
            let title = frozen
                .resolved_at
                .as_ref()
                .map(|value| format!("解析于 {}", time_label(value)))
                .unwrap_or_default();
            (
                summary,
                if frozen.prompt.is_empty() {
                    "（创建时未保存正文）".to_string()
                } else {
                    frozen.prompt.clone()
                },
                true,
                title,
            )
        }
    };

    let timeline: Vec<TimelineRow> = events
        .iter()
        .filter(|event| lifecycle_label(&event.event_type).is_some())
        .rev()
        .take(8)
        .map(|event| TimelineRow {
            label: lifecycle_label(&event.event_type)
                .unwrap_or_default()
                .to_string(),
            summary: event_summary(event),
            time: time_label(&event.created_at),
        })
        .collect();

    let rendered_events: Vec<EventRow> = events
        .iter()
        .rev()
        .take(60)
        .map(|event| EventRow {
            time: time_label(&event.created_at),
            kind: event.event_type.clone(),
            summary: event_summary(event),
            raw: serde_json::to_string_pretty(&event.payload)
                .unwrap_or_else(|_| String::new()),
        })
        .collect();

    DetailView {
        id: agent.agent.id.clone(),
        name: agent.agent.name.clone(),
        carrier_label: carrier_label(carriers, carrier_id),
        carrier: carrier_label(carriers, carrier_id),
        status_label: status_label.to_string(),
        status_tone: status_tone.to_string(),
        created: time_label(&agent.agent.created_at),
        updated: time_label(&agent.agent.updated_at),
        model: model_label(&agent.agent),
        thinking: agent
            .agent
            .thinking
            .map(|level| level.as_str().to_string())
            .unwrap_or_else(|| "默认".to_string()),
        process: match agent.agent.pid {
            Some(pid) => format!("PID {pid}"),
            None => short(&agent.agent.session_id, 24),
        },
        cwd: agent.agent.cwd.display().to_string(),
        skill_policy_label: skill_policy_label(agent),
        skill_chips,
        prompt_summary,
        prompt_title,
        prompt_chips,
        prompt_text,
        prompt_details_visible,
        has_error: agent.agent.last_error.is_some(),
        last_error: agent.agent.last_error.clone().unwrap_or_default(),
        can_send,
        can_abort,
        can_stop,
        can_restart,
        placeholder: if busy_cli {
            "当前回合执行中，请等待完成后发送下一条消息。".to_string()
        } else {
            "向当前 Agent 发送消息…".to_string()
        },
        message_kinds,
        timeline_count: timeline.len(),
        timeline,
        events: rendered_events,
    }
    .with_verbose(verbose)
}

impl DetailView {
    fn with_verbose(mut self, verbose: bool) -> Self {
        if !verbose {
            for event in &mut self.events {
                event.raw.clear();
            }
        }
        self
    }
}

fn build_manager(status: Option<&Value>, warnings: &[String]) -> ManagerView {
    let Some(status) = status else {
        return ManagerView {
            status_text: warnings
                .first()
                .cloned()
                .unwrap_or_else(|| "Skills Manager 状态不可用".to_string()),
            tone: "unavailable".to_string(),
            connected: false,
            facts: Vec::new(),
            note: "未连接时继续使用平台已导入的快照。".to_string(),
            can_connect: false,
            can_sync: false,
            can_disconnect: false,
        };
    };
    let connected = status
        .get("connected")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let manager = status.get("manager").cloned().unwrap_or(Value::Null);
    let snapshot = status.get("snapshot").cloned().unwrap_or(Value::Null);
    let config = status.get("config").cloned().unwrap_or(Value::Null);
    let manager_available = manager
        .get("available")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let version = manager
        .get("version")
        .and_then(Value::as_str)
        .or_else(|| config.get("managerVersion").and_then(Value::as_str));
    let status_text = if connected {
        let count = snapshot.get("skillCount").and_then(Value::as_u64);
        format!(
            "已连接 · {} · {}",
            version.unwrap_or("版本未知"),
            count
                .map(|count| format!("{count} 份快照"))
                .unwrap_or_else(|| "快照未知".to_string())
        )
    } else if manager_available {
        "未连接（需手动连接，不会自动连接全局库）".to_string()
    } else {
        manager
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("Skills Manager 不可用")
            .to_string()
    };

    let mut facts = Vec::new();
    if let Some(cli_path) = manager.get("cliPath").and_then(Value::as_str) {
        facts.push(Fact {
            label: "CLI".to_string(),
            value: cli_path.to_string(),
        });
    }
    if let Some(synced_at) = snapshot.get("syncedAt").and_then(Value::as_str) {
        facts.push(Fact {
            label: "最近同步".to_string(),
            value: time_label(synced_at),
        });
    }
    if let Some(digest) = snapshot.get("digest").and_then(Value::as_str) {
        facts.push(Fact {
            label: "快照摘要".to_string(),
            value: short(digest, 24),
        });
    }
    if let Some(selection) = config.get("selection")
        && !selection.is_null()
    {
        facts.push(Fact {
            label: "选择范围".to_string(),
            value: selection.to_string(),
        });
    }
    if let Some(drift) = status.get("drift") {
        let added = drift
            .get("added")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        let removed = drift
            .get("removed")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        facts.push(Fact {
            label: "变更".to_string(),
            value: format!("新增 {added} · 移除 {removed}"),
        });
    }

    ManagerView {
        status_text,
        tone: if connected {
            "connected".to_string()
        } else if manager_available {
            "idle".to_string()
        } else {
            "unavailable".to_string()
        },
        connected,
        facts,
        note: if connected {
            "同步只读取管理器目录，不会部署或改动管理器内容。创建 Agent 时会把所选 Skill 复制进平台私有存储并冻结；已创建的 Agent 不受断开或后续同步影响。".to_string()
        } else {
            "连接需要显式点击，平台不会自动连接或改写全局 Skill 库。未连接时继续使用平台已导入的快照。".to_string()
        },
        can_connect: manager_available,
        can_sync: connected,
        can_disconnect: connected,
    }
}

fn build_provider_blocks(snapshot: Option<&ProviderSnapshot>) -> Vec<ProviderAppBlock> {
    ProviderApp::ALL
        .iter()
        .map(|app| {
            let providers = providers_for(snapshot, *app);
            let available = snapshot.map(|value| value.switch.available).unwrap_or(false);
            let rows: Vec<ProviderRow> = providers
                .iter()
                .map(|provider| {
                    let ready = provider_ready(provider);
                    let badge = if !ready {
                        "未配置".to_string()
                    } else if *app == ProviderApp::Pi {
                        "按实例选择".to_string()
                    } else if provider.is_current.unwrap_or(false) {
                        "当前默认".to_string()
                    } else {
                        "可切换".to_string()
                    };
                    let badge_class = if !ready {
                        " unconfigured"
                    } else if provider.is_current.unwrap_or(false) && *app != ProviderApp::Pi {
                        " current"
                    } else {
                        ""
                    };
                    let mut facts = vec![
                        Fact {
                            label: "API URL".to_string(),
                            value: safe_base_url(provider.base_url.as_deref()),
                        },
                        Fact {
                            label: "协议".to_string(),
                            value: provider
                                .api_format
                                .clone()
                                .unwrap_or_else(|| "未标注".to_string()),
                        },
                    ];
                    if *app == ProviderApp::Claude {
                        facts.push(Fact {
                            label: "认证方式".to_string(),
                            value: match provider.auth_mode {
                                Some(pihub_contracts::AuthMode::ApiKey) => {
                                    "Anthropic API Key · x-api-key".to_string()
                                }
                                Some(pihub_contracts::AuthMode::Bearer) => {
                                    "Bearer Token · Authorization".to_string()
                                }
                                None => "未标注".to_string(),
                            },
                        });
                    }
                    facts.push(Fact {
                        label: if *app == ProviderApp::Pi {
                            "预设模型".to_string()
                        } else {
                            "默认模型".to_string()
                        },
                        value: provider
                            .default_model
                            .clone()
                            .unwrap_or_else(|| "未设置".to_string()),
                    });
                    ProviderRow {
                        id: provider.id.clone(),
                        name: provider.name.clone(),
                        badge,
                        badge_class: badge_class.to_string(),
                        facts,
                        models_label: format!("模型列表 · {}", provider.model_list.len()),
                        models: provider.model_list.clone(),
                        unconfigured: !ready,
                        action_label: if !ready {
                            "配置后可用".to_string()
                        } else if provider.is_current.unwrap_or(false) {
                            "正在使用".to_string()
                        } else {
                            "设为默认 ↗".to_string()
                        },
                        action_enabled: ready
                            && !provider.is_current.unwrap_or(false)
                            && available,
                    }
                })
                .collect();
            ProviderAppBlock {
                app_id: app.as_str().to_string(),
                app_label: app_label(*app).to_string(),
                note: if *app == ProviderApp::Pi {
                    "Pi 的模型由创建 Agent 时的参数指定；这里没有全局默认供应商切换。".to_string()
                } else {
                    "切换此载体的当前默认配置。创建 Agent 时也可单独指定供应商和模型。".to_string()
                },
                count_label: format!("{} 个供应商", rows.len()),
                empty_text: if available {
                    "尚未配置供应商。".to_string()
                } else {
                    "当前无法读取供应商列表。".to_string()
                },
                rows,
            }
        })
        .collect()
}

/// Accepted `apiFormat` value -> display label, per carrier. This is the single
/// source of truth: the SSR form, the fallback default and the server-side
/// normalisation all read it.
pub fn api_formats_for(app: ProviderApp) -> &'static [(&'static str, &'static str)] {
    match app {
        ProviderApp::Pi => &[
            ("openai-completions", "OpenAI Chat Completions"),
            ("openai-responses", "OpenAI Responses"),
            ("anthropic-messages", "Anthropic Messages"),
        ],
        ProviderApp::Codex => &[("responses", "OpenAI Responses")],
        ProviderApp::Claude => &[("anthropic", "Anthropic Messages")],
    }
}

/// The format a carrier uses when the request does not pick one.
pub fn default_api_format(app: ProviderApp) -> &'static str {
    match app {
        ProviderApp::Pi => "openai-completions",
        ProviderApp::Codex => "responses",
        ProviderApp::Claude => "anthropic",
    }
}

/// Map a submitted value to a format the carrier accepts; anything foreign (for
/// example a Pi format sent while Codex is selected) falls back to that
/// carrier's default instead of being forwarded.
pub fn normalize_api_format(app: ProviderApp, submitted: Option<&str>) -> String {
    let submitted = submitted.map(str::trim).unwrap_or_default();
    if api_formats_for(app)
        .iter()
        .any(|(value, _)| *value == submitted)
    {
        return submitted.to_string();
    }
    default_api_format(app).to_string()
}

fn build_api_formats() -> Vec<ApiFormatOption> {
    // The form starts on the first carrier, so only that carrier's formats are
    // visible without scripting; `data-app` lets the browser glue re-scope the
    // list, and the server normalises whatever finally arrives.
    // The provider dialog's carrier select starts on Pi.
    let initial = ProviderApp::Pi;
    ProviderApp::ALL
        .iter()
        .flat_map(|app| {
            api_formats_for(*app)
                .iter()
                .map(move |(value, label)| ApiFormatOption {
                    app: app.as_str().to_string(),
                    value: value.to_string(),
                    label: label.to_string(),
                    is_default: *value == default_api_format(*app),
                    hidden: *app != initial,
                })
        })
        .collect()
}

fn build_claude_auth_modes() -> Vec<Choice> {
    vec![
        Choice {
            value: "api-key".to_string(),
            label: "Anthropic API Key · x-api-key".to_string(),
            selected: true,
            disabled: false,
        },
        Choice {
            value: "bearer".to_string(),
            label: "Bearer Token · Authorization".to_string(),
            selected: false,
            disabled: false,
        },
    ]
}

/// Convenience for other modules that need the manager action label.
pub fn manager_action_label(action: SkillManagerAction) -> &'static str {
    match action {
        SkillManagerAction::Status => "状态",
        SkillManagerAction::Connect => "连接 Skills Manager",
        SkillManagerAction::Sync => "同步快照",
        SkillManagerAction::Disconnect => "断开连接",
    }
}
