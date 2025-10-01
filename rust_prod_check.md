# Rust Production Readiness Checklist

This document provides a comprehensive checklist for validating that the Rust implementation of `index-mcp` is production-ready. Use this checklist before promoting the Rust server from experimental to the default runtime or when preparing for a production deployment.

## Build & Compilation

- [ ] **Clean workspace build succeeds**
  ```bash
  cargo clean
  cargo build --release --workspace
  ```
  - Verifies all dependencies resolve correctly
  - Ensures no compilation errors or warnings that would block production use

- [ ] **Native addon builds successfully** (for Node runtime fallback)
  ```bash
  cd crates/index_mcp_native
  npm install
  npm run build
  ```
  - Required for the legacy Node runtime
  - Ensures FFI bindings are correctly generated

- [ ] **Linting passes without warnings**
  ```bash
  cargo clippy --workspace --all-targets -- -D warnings
  cargo fmt --all -- --check
  ```
  - Enforces code quality standards
  - Catches common bugs and anti-patterns

- [ ] **Dependencies are up-to-date and audited**
  ```bash
  cargo update --dry-run
  cargo audit
  ```
  - No known security vulnerabilities
  - All dependencies are actively maintained

## Core Functionality Validation

### Ingestion Pipeline

- [ ] **Basic ingestion completes successfully**
  - Test on a small repository (< 100 files)
  - Verify SQLite database is created with expected schema
  - Check that file metadata (path, size, modified, hash) is accurate

- [ ] **Large repository ingestion**
  - Test on a medium-sized repository (1,000-10,000 files)
  - Monitor memory usage stays within acceptable bounds
  - Verify all files are processed without panics or hangs

- [ ] **Filesystem crawling respects ignore patterns**
  - Verify `.gitignore` patterns are honored
  - Confirm `exclude` globs work correctly
  - Test that binary files are detected and handled appropriately

- [ ] **File content storage**
  - When enabled, verify content is stored accurately in SQLite
  - Confirm content can be retrieved for subsequent operations
  - Test with various file encodings (UTF-8, UTF-16, etc.)

- [ ] **Hash computation and change detection**
  - Verify SHA256 hashes match expected values
  - Confirm incremental ingestion only processes changed files
  - Test that hash collisions are handled (if applicable)

### Chunking & Tokenization

- [ ] **Content chunking produces valid output**
  - Verify `analyze_file_content` returns chunks with correct metadata
  - Check byte offsets (byteStart, byteEnd) are accurate
  - Confirm line numbers (lineStart, lineEnd) are correct

- [ ] **Batch analysis processes multiple files**
  - Test `analyzeFileContentBatch` with 10-100 files
  - Verify all files are chunked correctly
  - Confirm no memory leaks or excessive allocations

- [ ] **Chunk overlap is respected**
  - Verify overlapping content between chunks is preserved
  - Test with various overlap sizes (0, 50, 100 tokens)

- [ ] **Token counting accuracy**
  - Compare token counts with reference implementation
  - Verify chunk size limits are respected

### Embedding Generation

- [ ] **Embeddings are generated successfully**
  - Test with small batch (< 10 texts)
  - Verify embedding dimensions match model specification
  - Confirm embeddings are stored in SQLite correctly

- [ ] **Batch embedding performance**
  - Test with large batch (100+ texts)
  - Verify batching logic distributes work efficiently
  - Monitor memory usage during embedding generation

- [ ] **Embedding cache functions correctly**
  - Verify cache reduces redundant computations
  - Test cache invalidation when appropriate
  - Confirm `clearEmbeddingCache` clears cached data

- [ ] **Model loading and initialization**
  - Test with different embedding models
  - Verify model files are downloaded/cached correctly
  - Confirm graceful error handling for missing models

### Graph Extraction

- [ ] **TypeScript/JavaScript graph extraction**
  - Verify nodes (functions, classes, variables) are extracted
  - Confirm edges (imports, calls, references) are correct
  - Test with various TypeScript/JavaScript patterns

- [ ] **Graph queries return expected results**
  - Test `graph_neighbors` tool with various queries
  - Verify neighbor traversal works correctly
  - Confirm metadata (signatures, ranges) is accurate

- [ ] **Graph storage in SQLite**
  - Verify graph tables have expected schema
  - Confirm nodes and edges are indexed properly
  - Test that graph data can be queried efficiently

### Search & Retrieval

- [ ] **Semantic search returns relevant results**
  - Test `semantic_search` with various queries
  - Verify results are ranked by relevance
  - Confirm result metadata is accurate

- [ ] **Code lookup by path**
  - Test `code_lookup` with exact file paths
  - Verify file content is retrieved correctly
  - Test with non-existent paths (expect graceful error)

- [ ] **Context bundle generation**
  - Test `context_bundle` with various token budgets
  - Verify bundles respect token limits
  - Confirm relevant files/chunks are included

- [ ] **Repository timeline**
  - Test `repository_timeline` git integration
  - Verify commit history is parsed correctly
  - Confirm diff summaries are accurate

### Watch Mode

- [ ] **File watcher starts and monitors correctly**
  - Test with `--watch` flag
  - Verify debouncing works as expected
  - Confirm initial ingestion runs (when not disabled)

- [ ] **Change detection triggers re-ingestion**
  - Modify a file and verify it's re-indexed
  - Add a new file and confirm it's ingested
  - Delete a file and verify it's removed from index

- [ ] **Watcher cleanup on shutdown**
  - Test graceful shutdown (Ctrl+C)
  - Verify file system resources are released
  - Confirm no zombie processes remain

### Auto-Eviction

- [ ] **Eviction triggers at configured threshold**
  - Fill database to >80% of target size
  - Verify eviction runs automatically
  - Confirm least-used chunks/nodes are evicted first

- [ ] **Eviction statistics are accurate**
  - Verify evicted counts match actual deletions
  - Confirm database size is reduced post-eviction
  - Test that hits tracking influences eviction order

## Error Handling & Edge Cases

- [ ] **Invalid repository path**
  - Test with non-existent directory
  - Verify error message is descriptive
  - Confirm server doesn't crash

- [ ] **Permission denied errors**
  - Test with read-protected directory
  - Verify graceful error handling
  - Confirm other files still process

- [ ] **Oversized files**
  - Test with files exceeding `maxFileSizeBytes`
  - Verify files are skipped with appropriate reason
  - Confirm no memory exhaustion

- [ ] **Binary file handling**
  - Test with various binary formats (images, executables)
  - Verify binaries are detected correctly
  - Confirm no attempt to parse as text

- [ ] **Malformed file content**
  - Test with corrupted UTF-8 sequences
  - Verify error recovery mechanisms work
  - Confirm partial data is handled gracefully

- [ ] **SQLite database issues**
  - Test with locked database file
  - Verify appropriate retry logic
  - Confirm descriptive error messages

- [ ] **Network failures** (for remote proxies)
  - Simulate network timeout
  - Verify reconnection logic works
  - Confirm graceful degradation

## Performance Benchmarks

- [ ] **Ingestion throughput meets targets**
  - File scanning: ≥200 files/sec (8-core system)
  - Chunking: ≥30 MB/sec
  - Graph extraction: ≥10k lines/sec (TypeScript)
  - Capture baseline metrics for regression testing

- [ ] **Memory usage is bounded**
  - Monitor peak memory during large repository ingestion
  - Verify no memory leaks over extended runs
  - Confirm memory usage is predictable and documented

- [ ] **CPU utilization is efficient**
  - Verify parallelism scales with core count
  - Confirm no CPU spinning or busy-waiting
  - Test that async operations don't block

- [ ] **Database query performance**
  - Measure search query latency (target: <100ms for typical queries)
  - Verify indexes are used effectively
  - Confirm no full table scans on large databases

## Integration & Compatibility

- [ ] **MCP protocol compliance**
  - Test with MCP-compatible clients (Claude Desktop, etc.)
  - Verify tool discovery works correctly
  - Confirm all tool responses match expected schema

- [ ] **Database schema compatibility**
  - Verify Rust and Node servers can use same database
  - Test migration from Node-created database
  - Confirm schema upgrades are handled correctly

- [ ] **Prompt registration**
  - Verify prompts are registered correctly
  - Test prompt invocation from clients
  - Confirm prompt responses are valid

- [ ] **Remote MCP proxy integration**
  - Test with at least one remote MCP server
  - Verify tool forwarding works correctly
  - Confirm error handling for remote failures

## Deployment Readiness

- [ ] **Release binary is optimized**
  - Built with `--release` flag
  - Strip debug symbols if appropriate
  - Confirm binary size is reasonable

- [ ] **Platform support verified**
  - Test on Linux (x86_64, aarch64)
  - Test on macOS (x86_64, aarch64)
  - Test on Windows (x86_64)

- [ ] **Environment variables documented**
  - List all supported environment variables
  - Document their effects and default values
  - Provide examples of common configurations

- [ ] **Command-line interface is complete**
  - All flags are documented
  - Help text is clear and accurate
  - Examples are provided for common use cases

- [ ] **Logging and diagnostics**
  - Verify appropriate log levels (trace, debug, info, warn, error)
  - Confirm structured logging output
  - Test log filtering with environment variables

- [ ] **Graceful shutdown**
  - Test SIGINT (Ctrl+C) handling
  - Verify SIGTERM is handled
  - Confirm cleanup routines run

## CI/CD & Automation

- [ ] **CI pipeline builds successfully**
  - GitHub Actions workflow completes without errors
  - All platforms build correctly
  - Test coverage meets targets (if applicable)

- [ ] **Prebuilt binaries are published**
  - Release artifacts are uploaded
  - Checksums/signatures are provided
  - Installation instructions are clear

- [ ] **Automated testing**
  - Unit tests pass
  - Integration tests pass
  - Regression tests detect known issues

- [ ] **Documentation is current**
  - README reflects Rust as default runtime
  - Migration guide is complete
  - Troubleshooting section is helpful

## Migration from Node Runtime

- [ ] **Feature parity confirmed**
  - All Node runtime features are available in Rust
  - Tool signatures match
  - Response formats are identical

- [ ] **Migration path documented**
  - Step-by-step guide for existing users
  - Database migration instructions (if needed)
  - Rollback procedure documented

- [ ] **Deprecation timeline communicated**
  - Node runtime deprecation announced
  - Support timeline established
  - Migration deadline set (if applicable)

## Monitoring & Observability

- [ ] **Health check endpoint** (if applicable)
  - Verify endpoint responds correctly
  - Confirm health check includes critical dependencies
  - Test under various failure scenarios

- [ ] **Metrics collection**
  - Identify key metrics to track
  - Implement metric collection (if not already done)
  - Document how to access/export metrics

- [ ] **Error reporting**
  - Verify errors are logged with sufficient context
  - Test error aggregation (if applicable)
  - Confirm PII is not logged

- [ ] **Performance monitoring**
  - Identify performance bottlenecks
  - Document expected performance characteristics
  - Establish alerts for degradation

## Security

- [ ] **Dependency security audit passes**
  - No critical vulnerabilities (`cargo audit`)
  - All dependencies from trusted sources
  - Minimal attack surface

- [ ] **Input validation**
  - All user inputs are validated
  - Path traversal attacks prevented
  - SQL injection impossible (using parameterized queries)

- [ ] **File system access is restricted**
  - Only reads from specified repository root
  - No write access outside database directory
  - Symbolic link traversal is handled safely

- [ ] **Secrets management**
  - No hardcoded credentials
  - Environment variables used appropriately
  - Secrets are not logged

## Rollback Plan

- [ ] **Node runtime remains functional**
  - Legacy Node runtime can be activated
  - Instructions for switching back are documented
  - Database compatibility preserved

- [ ] **Rollback procedure tested**
  - Simulate production rollback
  - Verify minimal downtime
  - Confirm data integrity preserved

## Final Sign-off

- [ ] **All critical items above are checked**
- [ ] **Team review completed**
- [ ] **Documentation reviewed and approved**
- [ ] **Production deployment plan approved**
- [ ] **Monitoring and alerting configured**
- [ ] **Rollback plan tested and ready**

## Notes

Use this section to document any deviations from the checklist, known issues, or additional context:

---

**Date Completed:** _______________

**Reviewed By:** _______________

**Approved For Production:** [ ] Yes  [ ] No

**Deployment Date:** _______________
