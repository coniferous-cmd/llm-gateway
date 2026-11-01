use std::process::Command;

fn main() {
    let version = std::env::var("LLM_GATEWAY_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(git_describe)
        .unwrap_or_else(|| "dev".to_string());
    println!("cargo:rustc-env=LLM_GATEWAY_VERSION={version}");
    println!("cargo:rerun-if-env-changed=LLM_GATEWAY_VERSION");
    println!("cargo:rerun-if-changed=.git/HEAD");
}

fn git_describe() -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}
