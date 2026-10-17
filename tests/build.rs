use std::collections::HashMap;
use std::path::{Path, PathBuf};
use syn::visit::Visit;

fn test_only(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.path().is_ident("cfg")
            && attribute
                .parse_args::<syn::Path>()
                .is_ok_and(|path| path.is_ident("test"))
    })
}
fn imports(
    tree: &syn::UseTree,
    mut prefix: Vec<String>,
    aliases: &mut HashMap<String, Vec<String>>,
) {
    match tree {
        syn::UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            imports(&path.tree, prefix, aliases);
        }
        syn::UseTree::Name(name) => {
            prefix.push(name.ident.to_string());
            aliases.insert(name.ident.to_string(), prefix);
        }
        syn::UseTree::Rename(rename) => {
            prefix.push(rename.ident.to_string());
            aliases.insert(rename.rename.to_string(), prefix);
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                imports(item, prefix.clone(), aliases);
            }
        }
        syn::UseTree::Glob(_) => {}
    }
}
struct Guard<'a> {
    file: &'a Path,
    aliases: HashMap<String, Vec<String>>,
    native: bool,
    pty: bool,
}
impl Guard<'_> {
    fn reject(&self, path: &[String]) {
        let mut resolved = path.to_vec();
        for _ in 0..self.aliases.len() {
            let Some(first) = resolved.first() else {
                break;
            };
            let Some(prefix) = self.aliases.get(first) else {
                break;
            };
            if prefix.first() == Some(first) {
                break;
            }
            resolved = prefix
                .iter()
                .chain(resolved.iter().skip(1))
                .cloned()
                .collect();
        }
        let path = resolved.join("::");
        let raw_command = [
            "std::process::Command",
            "tokio::process::Command",
            "async_process::Command",
            "rmcp::transport::TokioChildProcess",
            "command_group::AsyncCommandGroup",
            "command_group::CommandGroup",
        ]
        .iter()
        .any(|name| path == *name || path.starts_with(&format!("{name}::")));
        let raw_native = [
            "libc::fork",
            "libc::vfork",
            "libc::posix_spawn",
            "libc::posix_spawnp",
            "nix::unistd::fork",
        ]
        .contains(&path.as_str());
        assert!(
            !["libc::fork", "libc::vfork", "nix::unistd::fork"].contains(&path.as_str()),
            "{} uses heavyweight process creation through {path}",
            self.file.display()
        );
        assert!(
            !(raw_command || raw_native) || self.native,
            "{} bypasses the canonical process launcher through {path}",
            self.file.display()
        );
        assert!(
            !path.starts_with("portable_pty::") || !path.ends_with("CommandBuilder") || self.pty,
            "{} bypasses the canonical terminal launcher",
            self.file.display()
        );
    }
}
impl<'ast> Visit<'ast> for Guard<'_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if !test_only(&item.attrs) {
            syn::visit::visit_item_mod(self, item);
        }
    }
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if !test_only(&item.attrs) {
            syn::visit::visit_item_fn(self, item);
        }
    }
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        if test_only(&item.attrs) {
            return;
        }
        imports(&item.tree, Vec::new(), &mut self.aliases);
        for path in self.aliases.values() {
            self.reject(path);
        }
    }
    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.reject(
            &path
                .segments
                .iter()
                .map(|part| part.ident.to_string())
                .collect::<Vec<_>>(),
        );
        syn::visit::visit_path(self, path);
    }
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        assert!(
            call.method != "request_restart",
            "{} delegates application relaunch outside the canonical launcher",
            self.file.display()
        );
        assert!(
            call.method != "pre_exec",
            "{} installs a fork-forcing Rust hook",
            self.file.display()
        );
        assert!(
            call.method != "spawn_command" || self.pty,
            "{} bypasses the canonical terminal launcher",
            self.file.display()
        );
        syn::visit::visit_expr_method_call(self, call);
    }
}
fn inspect_sources(root: &Path, directory: &Path) -> Result<(), Box<dyn std::error::Error>> {
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            inspect_sources(root, &path)?;
            continue;
        }
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        let relative = path.strip_prefix(root)?;
        let file = syn::parse_file(&std::fs::read_to_string(&path)?)?;
        let native = matches!(
            relative.to_str(),
            Some("process-env/src/launch/unix.rs" | "process-env/src/launch/windows.rs")
        );
        let pty = relative == Path::new("process-env/src/launch/pty.rs");
        Guard {
            file: relative,
            aliases: HashMap::new(),
            native,
            pty,
        }
        .visit_file(&file);
    }
    Ok(())
}
fn discover(root: &Path, directory: &Path) -> Result<(), Box<dyn std::error::Error>> {
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("non UTF-8 repository directory")?;
        if name.starts_with('.')
            || matches!(
                name,
                "target" | "node_modules" | "vendor" | "dist" | "build"
            )
        {
            continue;
        }
        if name == "src" {
            println!("cargo:rerun-if-changed={}", path.display());
            inspect_sources(root, &path)?;
        } else {
            discover(root, &path)?;
        }
    }
    Ok(())
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?)
        .parent()
        .ok_or("missing workspace root")?
        .to_owned();
    println!(
        "cargo:rerun-if-changed={}",
        root.join("Cargo.toml").display()
    );
    discover(&root, &root)
}
