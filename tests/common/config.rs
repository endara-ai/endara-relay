#![allow(dead_code)]

use std::path::Path;

/// Describes one endpoint entry in a relay TOML config.
struct EndpointEntry {
    name: String,
    transport: String,
    command: Option<String>,
    args: Vec<String>,
    url: Option<String>,
    oauth_server_url: Option<String>,
    env: Vec<(String, String)>,
}

/// Builder for generating relay TOML config strings.
pub struct ConfigBuilder {
    endpoints: Vec<EndpointEntry>,
    js_execution_mode: bool,
}

impl ConfigBuilder {
    pub fn new() -> Self {
        Self {
            endpoints: Vec::new(),
            js_execution_mode: false,
        }
    }

    /// Add a STDIO transport endpoint.
    pub fn add_stdio(mut self, name: &str, command: &str, args: &[&str]) -> Self {
        self.endpoints.push(EndpointEntry {
            name: name.to_string(),
            transport: "stdio".to_string(),
            command: Some(command.to_string()),
            args: args.iter().map(|s| s.to_string()).collect(),
            url: None,
            oauth_server_url: None,
            env: Vec::new(),
        });
        self
    }

    /// Add a STDIO transport endpoint with environment variables.
    pub fn add_stdio_with_env(
        mut self,
        name: &str,
        command: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Self {
        self.endpoints.push(EndpointEntry {
            name: name.to_string(),
            transport: "stdio".to_string(),
            command: Some(command.to_string()),
            args: args.iter().map(|s| s.to_string()).collect(),
            url: None,
            oauth_server_url: None,
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        });
        self
    }

    /// Add an HTTP transport endpoint.
    pub fn add_http(mut self, name: &str, url: &str) -> Self {
        self.endpoints.push(EndpointEntry {
            name: name.to_string(),
            transport: "http".to_string(),
            command: None,
            args: Vec::new(),
            url: Some(url.to_string()),
            oauth_server_url: None,
            env: Vec::new(),
        });
        self
    }

    /// Add an SSE transport endpoint.
    pub fn add_sse(mut self, name: &str, url: &str) -> Self {
        self.endpoints.push(EndpointEntry {
            name: name.to_string(),
            transport: "sse".to_string(),
            command: None,
            args: Vec::new(),
            url: Some(url.to_string()),
            oauth_server_url: None,
            env: Vec::new(),
        });
        self
    }

    /// Add an OAuth transport endpoint.
    pub fn add_oauth(mut self, name: &str, url: &str, oauth_server_url: Option<&str>) -> Self {
        self.endpoints.push(EndpointEntry {
            name: name.to_string(),
            transport: "oauth".to_string(),
            command: None,
            args: Vec::new(),
            url: Some(url.to_string()),
            oauth_server_url: oauth_server_url.map(|s| s.to_string()),
            env: Vec::new(),
        });
        self
    }

    /// Enable or disable JS execution mode.
    pub fn js_execution(mut self, enabled: bool) -> Self {
        self.js_execution_mode = enabled;
        self
    }

    /// Serialize the config to a TOML string.
    pub fn build(self) -> String {
        let mut out = String::new();
        out.push_str("[relay]\n");
        out.push_str("machine_name = \"integration-test\"\n");
        if self.js_execution_mode {
            out.push_str("local_js_execution = true\n");
        }
        out.push('\n');

        for ep in &self.endpoints {
            out.push_str("[[endpoints]]\n");
            out.push_str(&format!("name = \"{}\"\n", ep.name));
            out.push_str(&format!("transport = \"{}\"\n", ep.transport));
            if let Some(ref cmd) = ep.command {
                out.push_str(&format!("command = \"{}\"\n", cmd));
            }
            if !ep.args.is_empty() {
                let args_str: Vec<String> = ep.args.iter().map(|a| format!("\"{}\"", a)).collect();
                out.push_str(&format!("args = [{}]\n", args_str.join(", ")));
            }
            if let Some(ref url) = ep.url {
                out.push_str(&format!("url = \"{}\"\n", url));
            }
            if let Some(ref oauth_url) = ep.oauth_server_url {
                out.push_str(&format!("oauth_server_url = \"{}\"\n", oauth_url));
            }
            if !ep.env.is_empty() {
                out.push_str("env = { ");
                let pairs: Vec<String> = ep
                    .env
                    .iter()
                    .map(|(k, v)| format!("{} = \"{}\"", k, v))
                    .collect();
                out.push_str(&pairs.join(", "));
                out.push_str(" }\n");
            }
            out.push('\n');
        }

        out
    }
}

/// Convenience: config with just the everything MCP server.
pub fn everything_config() -> String {
    ConfigBuilder::new()
        .add_stdio(
            "everything",
            "npx",
            &["-y", "@modelcontextprotocol/server-everything"],
        )
        .build()
}

/// Convenience: config with everything + filesystem servers.
pub fn everything_plus_filesystem_config(root: &Path) -> String {
    ConfigBuilder::new()
        .add_stdio(
            "everything",
            "npx",
            &["-y", "@modelcontextprotocol/server-everything"],
        )
        .add_stdio(
            "filesystem",
            "npx",
            &[
                "-y",
                "@modelcontextprotocol/server-filesystem",
                &root.to_string_lossy(),
            ],
        )
        .build()
}

/// Convenience: config with everything + filesystem + memory servers.
pub fn three_server_config(fs_root: &Path) -> String {
    ConfigBuilder::new()
        .add_stdio(
            "everything",
            "npx",
            &["-y", "@modelcontextprotocol/server-everything"],
        )
        .add_stdio(
            "filesystem",
            "npx",
            &[
                "-y",
                "@modelcontextprotocol/server-filesystem",
                &fs_root.to_string_lossy(),
            ],
        )
        .add_stdio(
            "memory",
            "npx",
            &["-y", "@modelcontextprotocol/server-memory"],
        )
        .build()
}
