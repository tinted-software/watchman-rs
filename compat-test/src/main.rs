//! Compatibility harness: exercises our daemon using the *real*
//! `watchman_client` crate (the same one buck2 uses), talking BSER-v2 over
//! the actual UNIX domain socket. This is not part of the shipped
//! `watchman` binary -- see the workspace `Cargo.toml`.
//!
//! Usage:
//!   ../target/debug/watchman watch /path/to/root
//!   SOCK=$(../target/debug/watchman get-sockname | ...)
//!   WM_SOCK=$SOCK WM_ROOT=/path/to/root cargo run

use serde::Deserialize;
use watchman_client::prelude::*;

query_result_type! {
    struct NameAndType {
        name: NameField,
        file_type: FileTypeField,
        exists: ExistsField,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sock = std::env::var("WM_SOCK").expect("WM_SOCK env var required");
    let root_path = std::env::var("WM_ROOT").expect("WM_ROOT env var required");

    let client = Connector::new().unix_domain_socket(&sock).connect().await?;
    let resolved = client
        .resolve_root(CanonicalPath::canonicalize(&root_path)?)
        .await?;
    println!("resolved: {:?}", resolved);

    let result: QueryResult<NameAndType> = client
        .query(
            &resolved,
            QueryRequestCommon {
                expression: Some(Expr::Any(vec![
                    Expr::FileType(FileType::Regular),
                    Expr::FileType(FileType::Directory),
                ])),
                fields: vec!["name", "type", "exists"],
                ..Default::default()
            },
        )
        .await?;
    println!("fresh instance: {}", result.is_fresh_instance);
    for f in result.files.unwrap_or_default() {
        println!(
            "  {:?} type={:?} exists={:?}",
            f.name.into_inner(),
            *f.file_type,
            *f.exists
        );
    }
    println!("clock: {:?}", result.clock);

    Ok(())
}
