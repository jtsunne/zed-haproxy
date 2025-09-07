use zed_extension_api as zed;
use zed::{LanguageServerId, Command, Worktree};

struct HaproxyExtension;

impl zed::Extension for HaproxyExtension {
    fn new() -> Self {
        HaproxyExtension
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command, String> {
        // Debug logging (will show up in Zed's console)
        eprintln!("HAProxy Extension: language_server_command called for {}", language_server_id.as_ref());
        
        // Development paths (try relative to worktree)
        let dev_paths = [
            "target/release/haproxy-lsp",
            "target/debug/haproxy-lsp", 
            "bin/haproxy-lsp",
        ];
        
        for path in &dev_paths {
            if worktree.read_text_file(path).is_ok() {
                eprintln!("HAProxy Extension: Found dev binary at {}", path);
                return Ok(Command {
                    command: path.to_string(),
                    args: vec!["--stdio".to_string()],
                    env: Default::default(),
                });
            }
        }
        
        // Try to find haproxy-lsp in PATH (most reliable for installed version)
        eprintln!("HAProxy Extension: Using haproxy-lsp from PATH");
        return Ok(Command {
            command: "haproxy-lsp".to_string(),
            args: vec!["--stdio".to_string()],
            env: Default::default(),
        });
    }
}

zed::register_extension!(HaproxyExtension);
