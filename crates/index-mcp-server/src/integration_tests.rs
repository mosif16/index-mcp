use anyhow::Result;

use crate::bundle::SymbolSelector;
use crate::search::{Classification, SearchResultCoordinate, SearchSource};
use crate::test_support::TestWorkspace;

#[tokio::test]
async fn ingest_search_bundle_flow() -> Result<()> {
    let workspace = TestWorkspace::new()?;
    workspace.write_file(
        "src/lib.rs",
        r#"
        pub fn alpha(value: i32) -> i32 {
            value + 1
        }

        pub fn beta() -> i32 {
            alpha(41)
        }
        "#,
    )?;

    let ingest = workspace.ingest(|_| {}).await;
    assert_eq!(ingest.ingested_file_count, 1);
    assert!(ingest.embedded_chunk_count > 0);
    assert!(ingest.embedding_model.is_some());

    let search = workspace
        .semantic_search(|params| {
            params.query = "alpha(value: i32)".to_string();
        })
        .await;
    assert!(
        !search.results.is_empty(),
        "semantic search should return at least one match"
    );
    let top = &search.results[0];
    assert_eq!(top.path, "src/lib.rs");
    assert!(
        matches!(top.source, SearchSource::Embedding | SearchSource::Lexical),
        "unexpected search source: {:?}",
        top.source
    );

    let bundle = workspace
        .bundle(|params| {
            params.file = "src/lib.rs".to_string();
        })
        .await;
    assert!(
        bundle
            .definitions
            .iter()
            .any(|definition| definition.name == "alpha"),
        "bundle should surface definitions"
    );
    assert!(
        bundle
            .snippets
            .iter()
            .any(|snippet| snippet.content.contains("pub fn alpha")),
        "bundle should include snippet content"
    );

    Ok(())
}

#[tokio::test]
async fn semantic_search_respects_recent_hits() -> Result<()> {
    let workspace = TestWorkspace::new()?;
    workspace.write_file(
        "src/lib.rs",
        r#"
        pub fn alpha() -> i32 { 1 }
        pub fn gamma() -> i32 { alpha() }
        pub fn delta() -> i32 { alpha() + gamma() }
        "#,
    )?;

    workspace.ingest(|_| {}).await;

    let initial = workspace
        .semantic_search(|params| {
            params.query = "gamma".to_string();
            params.limit = Some(1);
        })
        .await;
    assert_eq!(initial.results.len(), 1);
    let seen = &initial.results[0];

    let filtered = workspace
        .semantic_search(|params| {
            params.query = "gamma".to_string();
            params.limit = Some(3);
            params.recent_hits = Some(vec![SearchResultCoordinate {
                path: seen.path.clone(),
                chunk_index: seen.chunk_index,
            }]);
        })
        .await;

    assert!(
        filtered
            .results
            .iter()
            .all(|result| { result.path != seen.path || result.chunk_index != seen.chunk_index }),
        "recent hit should have been filtered out"
    );

    Ok(())
}

#[tokio::test]
async fn bundle_focus_resolves_symbol_selector() -> Result<()> {
    let workspace = TestWorkspace::new()?;
    workspace.write_file(
        "src/lib.rs",
        r#"
        pub mod nested {
            pub fn target() -> &'static str { "ok" }
        }
        "#,
    )?;

    workspace.ingest(|_| {}).await;

    let bundle = workspace
        .bundle(|params| {
            params.file = "src/lib.rs".to_string();
            params.symbol = Some(SymbolSelector {
                name: "target".to_string(),
                kind: Some("function".to_string()),
            });
        })
        .await;

    let focus = bundle
        .focus_definition
        .as_ref()
        .map(|definition| definition.name.as_str());
    assert_eq!(focus, Some("target"));
    assert!(
        bundle.quick_links.iter().any(|link| link.label == "target"),
        "bundle should surface quick link for the target symbol"
    );

    Ok(())
}

#[tokio::test]
async fn swift_semantic_and_bundle_support() -> Result<()> {
    let workspace = TestWorkspace::new()?;
    workspace.write_file(
        "Sources/Greetings.swift",
        r#"
        public struct Greeter {
            public init() {}

            /// Returns a personalized greeting.
            public func greet(name: String) -> String {
                "Hello, \(name)!"
            }

            public convenience init?(rawValue: String) {
                self.init()
            }
        }

        public protocol Welcomer {
            func welcome(name: String) -> String
        }

        extension Greeter: Welcomer {
            /// Provides a welcome routed through the protocol witness.
            public func welcome(name: String) -> String {
                "Welcome, \(name)"
            }
        }
        "#,
    )?;

    let ingest = workspace.ingest(|_| {}).await;
    assert_eq!(ingest.ingested_file_count, 1);

    let search = workspace
        .semantic_search(|params| {
            params.query = "greet(name: String)".to_string();
            params.language = Some("Swift".to_string());
        })
        .await;
    assert!(
        !search.results.is_empty(),
        "semantic search should find Swift function"
    );
    let top = &search.results[0];
    assert_eq!(top.language.as_deref(), Some("Swift"));
    assert_eq!(top.classification, Classification::Function);
    assert!(
        top.content.contains("func greet"),
        "top result should include Swift function body"
    );

    let bundle = workspace
        .bundle(|params| {
            params.file = "Sources/Greetings.swift".to_string();
            params.symbol = Some(SymbolSelector {
                name: "greet".to_string(),
                kind: Some("function".to_string()),
            });
        })
        .await;
    assert!(
        bundle
            .definitions
            .iter()
            .any(|definition| definition.name == "greet"),
        "bundle should expose Swift definitions"
    );
    assert_eq!(
        bundle
            .focus_definition
            .as_ref()
            .map(|definition| definition.name.as_str()),
        Some("greet")
    );

    let greet_definition = bundle
        .definitions
        .iter()
        .find(|definition| definition.name == "greet")
        .expect("greet definition present");
    assert_eq!(greet_definition.visibility.as_deref(), Some("public"));
    assert!(greet_definition
        .docstring
        .as_deref()
        .is_some_and(|doc| doc.contains("personalized greeting")));

    let welcome_definition = bundle
        .definitions
        .iter()
        .find(|definition| definition.name == "welcome")
        .expect("welcome definition present");
    assert!(
        welcome_definition.visibility.is_some(),
        "extension method should record a visibility level"
    );

    Ok(())
}
