use mesh_llm_plugin::Plugin;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().any(|arg| arg == "--packaged-manifest") {
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
