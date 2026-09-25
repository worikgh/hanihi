use serde::{Deserialize, Serialize};
/// Definition of a Tool
use serde_json::Value;
use std::{borrow::Cow, sync::Arc};

/// Taken from `rmcp` crate
/// A JSON object type alias for convenient handling of JSON data.
///
/// This can be used for storing arbitrary JSON data in MCP messages.
#[allow(dead_code)]
pub type JsonObject<F = Value> = serde_json::Map<String, F>;

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
#[allow(dead_code)]
pub(crate) struct Tool {
    /// The name of the tool
    pub name: Cow<'static, str>,
    /// A human-readable title for the tool
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A description of what the tool does
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<Cow<'static, str>>,
    /// A JSON Schema object defining the expected parameters for the tool
    pub input_schema: Option<Value>,
    /// An optional JSON Schema object defining the structure of the tool's output
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Arc<JsonObject>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional additional tool information.
    pub annotations: Option<ToolAnnotations>,
    // Optional list of icons for the tool
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icons: Option<Vec<Icon>>,
    /// Optional additional metadata for this tool
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<String>,
}

impl Tool {
    #[allow(dead_code)]
    pub(crate) fn new(name: &str) -> Self {
        Self {
            name: name.to_string().into(),
            title: None,
            description: None,
            input_schema: None,
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn set_input_schema(&mut self, schema: Value) {
        self.input_schema = Some(schema);
    }
    #[allow(dead_code)]
    pub(crate) fn set_description(&mut self, description: &str) {
        self.description = Some(description.to_string().into());
    }
}
/// Additional properties describing a Tool to clients.
///
/// NOTE: all properties in ToolAnnotations are **hints**.
/// They are not guaranteed to provide a faithful description of
/// tool behavior (including descriptive properties like `title`).
///
/// Clients should never make tool use decisions based on ToolAnnotations
/// received from untrusted servers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ToolAnnotations {
    /// A human-readable title for the tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    /// If true, the tool does not modify its environment.
    ///
    /// Default: false
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,

    /// If true, the tool may perform destructive updates to its environment.
    /// If false, the tool performs only additive updates.
    ///
    /// (This property is meaningful only when `readOnlyHint == false`)
    ///
    /// Default: true
    /// A human-readable description of the tool's purpose.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,

    /// If true, calling the tool repeatedly with the same arguments
    /// will have no additional effect on the its environment.
    ///
    /// (This property is meaningful only when `readOnlyHint == false`)
    ///
    /// Default: false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,

    /// If true, this tool may interact with an "open world" of external
    /// entities. If false, the tool's domain of interaction is closed.
    /// For example, the world of a web search tool is open, whereas that
    /// of a memory tool is not.
    ///
    /// Default: true
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}
/// A URL pointing to an icon resource or a base64-encoded data URI.
///
/// Clients that support rendering icons MUST support at least the following MIME types:
/// - image/png - PNG images (safe, universal compatibility)
/// - image/jpeg (and image/jpg) - JPEG images (safe, universal compatibility)
///
/// Clients that support rendering icons SHOULD also support:
/// - image/svg+xml - SVG images (scalable but requires security precautions)
/// - image/webp - WebP images (modern, efficient format)
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Icon {
    /// A standard URI pointing to an icon resource
    pub src: String,
    /// Optional override if the server's MIME type is missing or generic
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Size specification, each string should be in WxH format (e.g., `\"48x48\"`, `\"96x96\"`) or `\"any\"` for scalable formats like SVG
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sizes: Option<Vec<String>>,
    // /// Optional specifier for the theme this icon is designed for
    // /// If not provided, the client should assume the icon can be used with any theme.
    // #[serde(skip_serializing_if = "Option::is_none")]
    // pub theme: Option<IconTheme>,
}

#[allow(dead_code)]
impl Icon {
    /// Create a new Icon with the given source URL.
    pub fn new(src: impl Into<String>) -> Self {
        Self {
            src: src.into(),
            mime_type: None,
            sizes: None,
            //theme: None,
        }
    }

    /// Set the MIME type.
    pub fn with_mime_type(mut self, mime_type: impl Into<String>) -> Self {
        self.mime_type = Some(mime_type.into());
        self
    }

    /// Set the sizes.
    pub fn with_sizes(mut self, sizes: Vec<String>) -> Self {
        self.sizes = Some(sizes);
        self
    }

    // /// Set the theme.
    // pub fn with_theme(mut self, theme: IconTheme) -> Self {
    //	self.theme = Some(theme);
    //	self
    // }
}
