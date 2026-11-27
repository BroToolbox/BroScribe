//! The reversible subset of CLI meeting edits, scoped to one MCP connection.

use super::input::*;
use super::*;

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SessionActionStatus {
    Open,
    Done,
    Dropped,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SessionActionEdit {
    /// New local person ID for the owner.
    owner: Option<String>,
    /// Clear the owner assignment.
    #[serde(default)]
    clear_owner: bool,
    /// New due date (YYYY-MM-DD).
    due: Option<String>,
    /// Clear the due date.
    #[serde(default)]
    clear_due: bool,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum SessionEdit {
    Rename {
        /// Meeting ID (m-…).
        meeting: String,
        /// New meeting title.
        title: String,
    },
    Tags {
        /// Meeting ID (m-…).
        meeting: String,
        /// Tags to add.
        #[serde(default)]
        add: Vec<String>,
        /// Tags to remove.
        #[serde(default)]
        remove: Vec<String>,
    },
    AttributeLines {
        /// Meeting ID (m-…).
        meeting: String,
        /// Exact fingerprints of the reviewed lines.
        fingerprints: Vec<Fingerprint>,
        /// Person reference. Omit to restore speaker attribution.
        person: Option<String>,
    },
    ActionStatus {
        /// Meeting ID (m-…).
        meeting: String,
        /// Action item ID.
        id: String,
        /// New action item status.
        status: SessionActionStatus,
    },
    EditAction {
        /// Meeting ID (m-…).
        meeting: String,
        /// Action item ID.
        id: String,
        /// New or cleared action item fields.
        edit: SessionActionEdit,
    },
    SpeakerReview {
        /// Meeting ID (m-…).
        meeting: String,
        /// Speaker ID from the meeting.
        speaker: String,
        /// True to dismiss the review; false to restore it.
        dismiss: bool,
        /// Optional speaker namespace and grouping revision from the displayed meeting.
        fence: Option<TranscriptFence>,
    },
    Name {
        /// Meeting ID (m-…).
        meeting: String,
        /// Speaker ID from the meeting.
        speaker: String,
        /// A person ID, name or e-mail.
        person: String,
        /// Optional speaker namespace and grouping revision from the displayed meeting.
        fence: Option<TranscriptFence>,
    },
    Undo,
    Redo,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SessionEditInput {
    /// Reversible edit, undo or redo in this MCP connection.
    edit: SessionEdit,
}

#[tool_router(router = session_writes, vis = "pub(in crate::mcp)")]
impl Server {
    #[tool(
        description = "Apply, undo or redo an identity or meeting edit in this MCP connection. Returns the CLI edit-session result and whether undo and redo are available.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn edit_meeting_session(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<SessionEditInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Identity {
                action: IdentityCmd::EditSession,
            },
            Some(json!(p.edit).to_string()),
        )
        .await
    }
}
