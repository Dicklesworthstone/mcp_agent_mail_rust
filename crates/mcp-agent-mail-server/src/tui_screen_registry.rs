//! Browser-safe screen identity and metadata for the Agent Mail TUI shell.
//!
//! This module deliberately contains no screen implementations, database
//! access, filesystem access, or runtime state. Native and browser runners use
//! the same registry so tab order, labels, shortcuts, categories, and help
//! content cannot drift between targets.

/// Identifies a TUI screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MailScreenId {
    Dashboard,
    Messages,
    Threads,
    Agents,
    Search,
    Reservations,
    ToolMetrics,
    SystemHealth,
    Timeline,
    Projects,
    Contacts,
    Explorer,
    Analytics,
    Attachments,
    ArchiveBrowser,
    Atc,
}

/// All screen IDs in display order.
pub const ALL_SCREEN_IDS: &[MailScreenId] = &[
    MailScreenId::Dashboard,
    MailScreenId::Messages,
    MailScreenId::Threads,
    MailScreenId::Agents,
    MailScreenId::Search,
    MailScreenId::Reservations,
    MailScreenId::ToolMetrics,
    MailScreenId::SystemHealth,
    MailScreenId::Timeline,
    MailScreenId::Projects,
    MailScreenId::Contacts,
    MailScreenId::Explorer,
    MailScreenId::Analytics,
    MailScreenId::Attachments,
    MailScreenId::ArchiveBrowser,
    MailScreenId::Atc,
];

/// Shifted number-row symbols used for direct jump bindings beyond screen 10.
///
/// Mapping: `!`=11, `@`=12, `#`=13, `$`=14, ... `(`=19.
pub const SHIFTED_DIGIT_JUMP_KEYS: &[char] = &['!', '@', '#', '$', '%', '^', '&', '*', '('];

fn screen_from_display_index(idx: usize) -> Option<MailScreenId> {
    if idx == 0 || idx > ALL_SCREEN_IDS.len() {
        None
    } else {
        Some(ALL_SCREEN_IDS[idx - 1])
    }
}

/// Return the direct jump key label for a 1-based display index.
///
/// - `1..=9` map to `"1"`..`"9"`
/// - `10` maps to `"0"`
/// - `11+` map to shifted symbols (`!`, `@`, `#`, ...)
#[must_use]
pub const fn jump_key_label_for_display_index(display_index: usize) -> Option<&'static str> {
    match display_index {
        1 => Some("1"),
        2 => Some("2"),
        3 => Some("3"),
        4 => Some("4"),
        5 => Some("5"),
        6 => Some("6"),
        7 => Some("7"),
        8 => Some("8"),
        9 => Some("9"),
        10 => Some("0"),
        11 => Some("!"),
        12 => Some("@"),
        13 => Some("#"),
        14 => Some("$"),
        15 => Some("%"),
        16 => Some("^"),
        17 => Some("&"),
        18 => Some("*"),
        19 => Some("("),
        _ => None,
    }
}

/// Return the direct jump key label for a screen, if one exists.
#[must_use]
pub fn jump_key_label_for_screen(id: MailScreenId) -> Option<&'static str> {
    jump_key_label_for_display_index(id.index() + 1)
}

/// Parse a jump key character into the corresponding screen.
///
/// Supports numeric keys and shifted number-row symbols for 11+ screens.
#[must_use]
pub fn screen_from_jump_key(key: char) -> Option<MailScreenId> {
    if key.is_ascii_digit() {
        let n = key.to_digit(10).map_or(0, |d| d as usize);
        return MailScreenId::from_number(n);
    }

    let shifted_offset = SHIFTED_DIGIT_JUMP_KEYS.iter().position(|&c| c == key)?;
    screen_from_display_index(11 + shifted_offset)
}

/// Human-readable key legend for direct jump navigation.
#[must_use]
pub fn jump_key_legend() -> String {
    let mut labels = vec!["1-9".to_string(), "0".to_string()];
    let extra = ALL_SCREEN_IDS.len().saturating_sub(10);
    labels.extend((0..extra).filter_map(|offset| {
        jump_key_label_for_display_index(11 + offset).map(ToString::to_string)
    }));
    labels.join(",")
}

impl MailScreenId {
    /// Returns the zero-based display index.
    #[must_use]
    pub fn index(self) -> usize {
        ALL_SCREEN_IDS
            .iter()
            .position(|&id| id == self)
            .unwrap_or(0)
    }

    /// Return the next screen in tab order (wraps).
    #[must_use]
    pub fn next(self) -> Self {
        let idx = self.index();
        ALL_SCREEN_IDS[(idx + 1) % ALL_SCREEN_IDS.len()]
    }

    /// Return the previous screen in tab order (wraps).
    #[must_use]
    pub fn prev(self) -> Self {
        let idx = self.index();
        let len = ALL_SCREEN_IDS.len();
        ALL_SCREEN_IDS[(idx + len - 1) % len]
    }

    /// Look up a screen by numeric jump index.
    #[must_use]
    pub fn from_number(n: usize) -> Option<Self> {
        let idx = if n == 0 { 10 } else { n };
        screen_from_display_index(idx)
    }

    /// Total number of registered screens.
    pub const COUNT: usize = ALL_SCREEN_IDS.len();

    /// Stable machine-readable identifier (snake_case) for this screen.
    #[must_use]
    pub const fn as_slug(self) -> &'static str {
        match self {
            Self::Dashboard => "dashboard",
            Self::Messages => "messages",
            Self::Threads => "threads",
            Self::Agents => "agents",
            Self::Search => "search",
            Self::Reservations => "reservations",
            Self::ToolMetrics => "tool_metrics",
            Self::SystemHealth => "system_health",
            Self::Timeline => "timeline",
            Self::Projects => "projects",
            Self::Contacts => "contacts",
            Self::Explorer => "explorer",
            Self::Analytics => "analytics",
            Self::Attachments => "attachments",
            Self::ArchiveBrowser => "archive_browser",
            Self::Atc => "atc",
        }
    }
}

/// Screen category for grouping in the help overlay and chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScreenCategory {
    Overview,
    Communication,
    Operations,
    System,
}

impl ScreenCategory {
    /// Short display label (max 4 chars) for compact UI.
    #[must_use]
    pub const fn short_label(self) -> &'static str {
        match self {
            Self::Overview => "Over",
            Self::Communication => "Comm",
            Self::Operations => "Ops",
            Self::System => "Sys",
        }
    }

    /// Full display label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Communication => "Communication",
            Self::Operations => "Operations",
            Self::System => "System",
        }
    }

    /// All category variants in display order.
    pub const ALL: &[Self] = &[
        Self::Overview,
        Self::Communication,
        Self::Operations,
        Self::System,
    ];
}

/// Static metadata for a screen.
#[derive(Debug, Clone)]
pub struct MailScreenMeta {
    pub id: MailScreenId,
    pub title: &'static str,
    pub short_label: &'static str,
    pub category: ScreenCategory,
    pub description: &'static str,
    pub help_markdown: &'static str,
}

const DASHBOARD_HELP_MARKDOWN: &str = include_str!("tui_screens/dashboard/help.md");
const MESSAGES_HELP_MARKDOWN: &str = include_str!("tui_screens/messages/help.md");
const THREADS_HELP_MARKDOWN: &str = include_str!("tui_screens/threads/help.md");
const AGENTS_HELP_MARKDOWN: &str = include_str!("tui_screens/agents/help.md");
const SEARCH_HELP_MARKDOWN: &str = include_str!("tui_screens/search/help.md");
const RESERVATIONS_HELP_MARKDOWN: &str = include_str!("tui_screens/reservations/help.md");
const TOOL_METRICS_HELP_MARKDOWN: &str = include_str!("tui_screens/tool_metrics/help.md");
const SYSTEM_HEALTH_HELP_MARKDOWN: &str = include_str!("tui_screens/system_health/help.md");
const TIMELINE_HELP_MARKDOWN: &str = include_str!("tui_screens/timeline/help.md");
const PROJECTS_HELP_MARKDOWN: &str = include_str!("tui_screens/projects/help.md");
const CONTACTS_HELP_MARKDOWN: &str = include_str!("tui_screens/contacts/help.md");
const EXPLORER_HELP_MARKDOWN: &str = include_str!("tui_screens/explorer/help.md");
const ANALYTICS_HELP_MARKDOWN: &str = include_str!("tui_screens/analytics/help.md");
const ATTACHMENTS_HELP_MARKDOWN: &str = include_str!("tui_screens/attachments/help.md");
const ARCHIVE_BROWSER_HELP_MARKDOWN: &str = include_str!("tui_screens/archive_browser/help.md");
const ATC_HELP_MARKDOWN: &str = include_str!("tui_screens/atc/help.md");

/// Static registry of all screens with their metadata.
pub const MAIL_SCREEN_REGISTRY: &[MailScreenMeta] = &[
    MailScreenMeta {
        id: MailScreenId::Dashboard,
        title: "Dashboard",
        short_label: "Dash",
        category: ScreenCategory::Overview,
        description: "Real-time operational overview with live event stream",
        help_markdown: DASHBOARD_HELP_MARKDOWN,
    },
    MailScreenMeta {
        id: MailScreenId::Messages,
        title: "Messages",
        short_label: "Msg",
        category: ScreenCategory::Communication,
        description: "Search and browse messages with detail panel",
        help_markdown: MESSAGES_HELP_MARKDOWN,
    },
    MailScreenMeta {
        id: MailScreenId::Threads,
        title: "Threads",
        short_label: "Threads",
        category: ScreenCategory::Communication,
        description: "Thread explorer with conversation view",
        help_markdown: THREADS_HELP_MARKDOWN,
    },
    MailScreenMeta {
        id: MailScreenId::Agents,
        title: "Agents",
        short_label: "Agents",
        category: ScreenCategory::Operations,
        description: "Agent roster with status and activity",
        help_markdown: AGENTS_HELP_MARKDOWN,
    },
    MailScreenMeta {
        id: MailScreenId::Search,
        title: "Search",
        short_label: "Find",
        category: ScreenCategory::Communication,
        description: "Unified search across messages, agents, and projects with facet filters",
        help_markdown: SEARCH_HELP_MARKDOWN,
    },
    MailScreenMeta {
        id: MailScreenId::Reservations,
        title: "Reservations",
        short_label: "Reserv",
        category: ScreenCategory::Operations,
        description: "File reservation conflicts and status",
        help_markdown: RESERVATIONS_HELP_MARKDOWN,
    },
    MailScreenMeta {
        id: MailScreenId::ToolMetrics,
        title: "Tool Metrics",
        short_label: "Tools",
        category: ScreenCategory::System,
        description: "Per-tool call counts, latency, and error rates",
        help_markdown: TOOL_METRICS_HELP_MARKDOWN,
    },
    MailScreenMeta {
        id: MailScreenId::SystemHealth,
        title: "System Health",
        short_label: "Health",
        category: ScreenCategory::System,
        description: "Database, queue, and connection diagnostics",
        help_markdown: SYSTEM_HEALTH_HELP_MARKDOWN,
    },
    MailScreenMeta {
        id: MailScreenId::Timeline,
        title: "Timeline",
        short_label: "Time",
        category: ScreenCategory::Overview,
        description: "Chronological event timeline with cursor + inspector",
        help_markdown: TIMELINE_HELP_MARKDOWN,
    },
    MailScreenMeta {
        id: MailScreenId::Projects,
        title: "Projects",
        short_label: "Proj",
        category: ScreenCategory::Overview,
        description: "Project browser with per-project stats and detail",
        help_markdown: PROJECTS_HELP_MARKDOWN,
    },
    MailScreenMeta {
        id: MailScreenId::Contacts,
        title: "Contacts",
        short_label: "Links",
        category: ScreenCategory::Communication,
        description: "Cross-agent contact links and policy display",
        help_markdown: CONTACTS_HELP_MARKDOWN,
    },
    MailScreenMeta {
        id: MailScreenId::Explorer,
        title: "Explorer",
        short_label: "Explore",
        category: ScreenCategory::Communication,
        description: "Unified inbox/outbox explorer with direction, grouping, and ack filters",
        help_markdown: EXPLORER_HELP_MARKDOWN,
    },
    MailScreenMeta {
        id: MailScreenId::Analytics,
        title: "Analytics",
        short_label: "Insight",
        category: ScreenCategory::System,
        description: "Anomaly insight feed with confidence scoring and actionable next steps",
        help_markdown: ANALYTICS_HELP_MARKDOWN,
    },
    MailScreenMeta {
        id: MailScreenId::Attachments,
        title: "Attachments",
        short_label: "Attach",
        category: ScreenCategory::Communication,
        description: "Attachment browser with inline preview and source provenance trails",
        help_markdown: ATTACHMENTS_HELP_MARKDOWN,
    },
    MailScreenMeta {
        id: MailScreenId::ArchiveBrowser,
        title: "Archive Browser",
        short_label: "Archive",
        category: ScreenCategory::Operations,
        description: "Two-pane Git archive browser with directory tree and file content preview",
        help_markdown: ARCHIVE_BROWSER_HELP_MARKDOWN,
    },
    MailScreenMeta {
        id: MailScreenId::Atc,
        title: "ATC",
        short_label: "ATC",
        category: ScreenCategory::System,
        description: "Air Traffic Controller decision engine with agent liveness, conflict, and evidence ledger",
        help_markdown: ATC_HELP_MARKDOWN,
    },
];

/// Look up metadata for a screen ID.
#[must_use]
pub fn screen_meta(id: MailScreenId) -> &'static MailScreenMeta {
    MAIL_SCREEN_REGISTRY
        .iter()
        .find(|meta| meta.id == id)
        .unwrap_or_else(|| unreachable!())
}

/// All screen IDs in display order.
#[must_use]
pub const fn screen_ids() -> &'static [MailScreenId] {
    ALL_SCREEN_IDS
}
