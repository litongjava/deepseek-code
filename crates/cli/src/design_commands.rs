//! Explicit Design account/project commands; sync plans never upload implicitly.
use crate::{args::Cli, session::Session};
use anyhow::{ensure, Context, Result};
use deepseek_code_core::{
    runtime::RuntimePaths,
    services::{anthropic_auth, design},
};
fn show(plan: &design::UploadPlan) -> Result<()> {
    eprintln!(
        "Design upload plan {} [{}]\nProject: {}\nSource: {}\n{} files; {} uploaded",
        plan.id,
        plan.status,
        plan.project_id,
        plan.directory.display(),
        plan.files.len(),
        plan.uploaded
    );
    for path in &plan.deletes {
        eprintln!("  DELETE {path}");
    }
    for file in &plan.files {
        eprintln!("  {} ({} bytes)", file.path, file.bytes);
    }
    if let Some(error) = &plan.error {
        eprintln!("{error}");
    }
    eprintln!("The app registers preview cards from @dsCard headers when the project opens. Review converter render grades and the full write/delete list before /design-sync upload {} --verified-bundle.",plan.id);
    Ok(())
}
pub async fn handle(
    _cli: &Cli,
    command: &str,
    argument: &str,
    paths: &RuntimePaths,
    _session: &mut Session,
) -> Result<bool> {
    handle_command(command, argument, paths).await
}

async fn handle_command(command: &str, argument: &str, paths: &RuntimePaths) -> Result<bool> {
    match command {
        "/design-login" => {
            ensure!(argument.trim().is_empty(), "Usage: /design-login");
            let flow = design::begin_login().await?;
            eprintln!(
                "Authorize separate Design read/write access:\n{}",
                flow.authorization_url
            );
            if let Err(error) = anthropic_auth::open_browser(&flow.authorization_url) {
                eprintln!("{error}; open the URL manually.");
            }
            let status = tokio::select! {r=design::finish_login(&paths.data_dir,flow)=>r?,_=tokio::signal::ctrl_c()=>anyhow::bail!("Design login cancelled")};
            eprintln!("Design authorized: {}", status.authenticated);
        }
        "/design" => {
            let words: Vec<_> = argument.split_whitespace().collect();
            let value = match words.as_slice() {
                [] | ["status"] => serde_json::to_value(design::status(&paths.data_dir)?)?,
                ["list"] => design::list(&paths.data_dir).await?,
                ["project", id] => design::project(&paths.data_dir, id).await?,
                ["grants"] => design::grants(&paths.data_dir).await?,
                ["grant", id] => design::grant(&paths.data_dir, id).await?,
                ["consent"] => design::consent(&paths.data_dir, true).await?,
                ["revoke"] => design::consent(&paths.data_dir, false).await?,
                ["logout"] => {
                    design::logout(&paths.data_dir).await?;
                    serde_json::json!({"local_design_credential_removed":true})
                }
                _ => anyhow::bail!(
                    "Usage: /design [status|list|project ID|grants|grant ID|consent|revoke|logout]"
                ),
            };
            eprintln!("{}", serde_json::to_string_pretty(&value)?);
        }
        "/design-sync" => {
            let mut words = argument.trim().splitn(3, ' ');
            match words.next().unwrap_or(""){
                "setup"=>{
                    let option=words.next();ensure!((option.is_none()||option==Some("--browser"))&&words.next().is_none(),"Usage: /design-sync setup [--browser]");
                    eprintln!("Installing pinned Design converter dependencies{}.",if option.is_some(){" and Chromium"}else{""});
                    let directory=tokio::select!{r=design::setup_converter(&paths.data_dir,option.is_some())=>r?,_=tokio::signal::ctrl_c()=>anyhow::bail!("Design converter setup cancelled")};
                    eprintln!("Converter installed: {}",directory.display());
                },
                "build"=>{
                    let recipe_path=argument.trim().strip_prefix("build").unwrap().trim().trim_matches('"');
                    let recipe:design::ConvertRecipe=serde_json::from_slice(&std::fs::read(recipe_path)?)?;
                    eprintln!("Running Design converter and validators; logs are saved under design-conversions.");
                    let result=tokio::select!{r=design::convert(&paths.data_dir,&recipe)=>r?,_=tokio::signal::ctrl_c()=>anyhow::bail!("Design conversion cancelled")};
                    eprintln!("{}",serde_json::to_string_pretty(&result)?);
                },
                "remote-plan"=>{let id=words.next().context("Missing plan ID")?;show(&design::remote_plan(&paths.data_dir,id).await?)?;},
                "snapshot"=>{let project=words.next().context("Missing project ID")?;let destination=words.next().context("Missing destination filename")?.trim_matches('"');design::download_sidecar(&paths.data_dir,project,std::path::Path::new(destination)).await?;eprintln!("Remote sidecar saved to {destination}");},
                "plan"=>{let project=words.next().context("Usage: /design-sync plan PROJECT_ID BUNDLE_DIRECTORY")?;let directory=words.next().context("Missing bundle directory")?.trim().trim_matches('"');show(&design::plan(&paths.data_dir,project,std::path::Path::new(directory))?)?;},
                "show"=>{let id=words.next().context("Missing plan ID")?;show(&design::load_plan(&paths.data_dir,id)?)?;},
                "upload"=>{let id=words.next().context("Missing plan ID")?;ensure!(words.next()==Some("--verified-bundle"),"Review /design-sync show ID and validate the prebuilt bundle, then use /design-sync upload ID --verified-bundle");let plan=tokio::select!{r=design::upload(&paths.data_dir,id)=>r?,_=tokio::signal::ctrl_c()=>anyhow::bail!("Design upload interrupted; inspect remote state and plan before retrying")};show(&plan)?;ensure!(plan.status=="files_uploaded","Design upload failed");},
                _=>eprintln!("/design-sync setup [--browser]\n/design-sync build RECIPE_JSON\n/design-sync plan PROJECT_ID BUNDLE_DIRECTORY\n/design-sync show PLAN_ID\n/design-sync upload PLAN_ID --verified-bundle\nUse /design-sync build RECIPE_JSON to run the bundled Node converter (recipe.converter optionally overrides it) with browser validation. For re-sync, snapshot PROJECT FILE, rebuild with that remote snapshot, then remote-plan PLAN_ID before uploading. Review pending visual grades before --verified-bundle."),
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn converter_commands_dispatch_and_validate_before_external_actions() {
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        assert!(handle_command("/design-sync", "", &paths).await.unwrap());
        assert!(handle_command("/design-sync", "setup --invalid", &paths)
            .await
            .unwrap_err()
            .to_string()
            .contains("Usage:"));
        assert!(
            handle_command("/design-sync", "setup --browser unexpected", &paths)
                .await
                .is_err()
        );
        assert!(
            handle_command("/design-sync", "build absent-recipe.json", &paths)
                .await
                .is_err()
        );
        assert!(handle_command("/design-sync", "snapshot project", &paths)
            .await
            .unwrap_err()
            .to_string()
            .contains("destination"));
        assert!(handle_command("/design-sync", "remote-plan", &paths)
            .await
            .unwrap_err()
            .to_string()
            .contains("plan ID"));
        assert!(!handle_command("/other", "", &paths).await.unwrap());
    }
}
