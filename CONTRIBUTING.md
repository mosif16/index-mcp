# Contributing to index-mcp

## Code Organization

The codebase is structured into several key modules:

### Source Files

- **server.ts** (1893 lines) - Main MCP server implementation with tool definitions
- **ingest.ts** (1188 lines) - Codebase ingestion and indexing logic
- **context-bundle.ts** (587 lines) - Context bundle generation with token budgets
- **remote-proxy.ts** (579 lines) - Remote MCP server proxying
- **git-timeline.ts** (562 lines) - Git timeline analysis

### Module Size Considerations

Several source files exceed the recommended 300-500 line limit for optimal readability:

- `server.ts`: 1893 lines - Contains all MCP tool definitions and handlers
- `ingest.ts`: 1188 lines - Complex ingestion pipeline with native module integration
- `context-bundle.ts`: 587 lines - Comprehensive context bundling logic
- `remote-proxy.ts`: 579 lines - Remote server management
- `git-timeline.ts`: 562 lines - Timeline generation and analysis

These files are intentionally kept consolidated to maintain coherent workflows and reduce unnecessary indirection. Future refactoring could split these into smaller, more focused modules if maintainability becomes an issue.

## Best Practices

This codebase follows MCP best practices:

- ✅ Uses Pino for structured logging with configurable output
- ✅ Implements comprehensive parameter aliasing for agent flexibility
- ✅ Provides actionable error messages with remediation steps
- ✅ Uses industry-standard token estimation (4 chars/token)
- ✅ Maintains stdout purity (no console.log statements)
- ✅ Includes proper shebang line in CLI entrypoint
- ✅ Uses compiled JavaScript for production execution

## Building and Testing

```bash
# Install dependencies
npm install

# Build the project
npm run build

# Run linter
npm run lint

# Start the server
npm start

# Development mode with live reload
npm run dev
```

## Adding New Tools

When adding new MCP tools:

1. Add tool schema to `server.ts`
2. Document all parameter aliases in descriptions
3. Implement parameter normalization in `input-normalizer.ts`
4. Use the existing error handling pattern with actionable messages
5. Update tool list in README.md
