use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A base64-encoded image attached to a user message.
///
/// The session record keeps the bytes inline so a resumed conversation can
/// send them again. The provider layer expands the images into OpenAI
/// `content` parts at the wire boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    /// MIME type, e.g. `image/png`.
    pub media_type: String,
    /// Base64 without a data-URL prefix.
    pub data: String,
}

impl Image {
    pub fn new(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            media_type: media_type.into(),
            data: data.into(),
        }
    }

    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data)
    }
}

/// The one message type of the whole system.
///
/// It doubles as the wire format (OpenAI chat completions) and the session
/// record format, so nothing has to be converted at the boundaries.
/// `reasoning` is kept because DeepSeek requires `reasoning_content` to be
/// passed back on every assistant turn while tools are in play.
/// `images` is only used on user messages; an image-only turn is allowed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON string as produced by the model (may be empty).
    pub arguments: String,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(text.into()),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            images: Vec::new(),
        }
    }

    /// A user message with images; the text may be empty.
    pub fn user_with_images(text: impl Into<String>, images: Vec<Image>) -> Self {
        let mut message = Self::user(text);
        message.images = images;
        message
    }

    pub fn assistant(
        text: Option<String>,
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: text,
            reasoning,
            tool_calls,
            tool_call_id: None,
            images: Vec::new(),
        }
    }

    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
            images: Vec::new(),
        }
    }

    pub fn text(&self) -> &str {
        self.content.as_deref().unwrap_or("")
    }
}
