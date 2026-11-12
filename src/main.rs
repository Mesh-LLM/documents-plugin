use mesh_llm_plugin::Plugin;

fn main() -> anyhow::Result<std::process::ExitCode> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(mesh_documents_plugin::tika::WORKER_FLAG) {
        return Ok(mesh_documents_plugin::tika::run_worker(&args[2..]));
    }
    tokio::runtime::Runtime::new()?.block_on(serve(&args))?;
    Ok(std::process::ExitCode::SUCCESS)
}

async fn serve(args: &[String]) -> anyhow::Result<()> {
    if args.iter().any(|arg| arg == "--packaged-manifest") {
        // Written into the release archive as plugin-manifest.json, so the host
        // can validate `[plugin.settings]` before the plugin starts.
        let manifest = mesh_documents_plugin::plugin()
            .manifest()
            .ok_or_else(|| anyhow::anyhow!("plugin has no manifest"))?;
        let json = mesh_llm_plugin::package_manifest_json(&manifest)?;
        std::io::Write::write_all(&mut std::io::stdout(), json.as_bytes())?;
        return Ok(());
    }
    mesh_documents_plugin::run().await
}
