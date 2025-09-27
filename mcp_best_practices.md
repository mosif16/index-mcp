MCP Tool Creation Best Practices

Overview – Why MCP Tools Are Different

Model Context Protocol (MCP) defines a standard interface for connecting language‑model agents to external systems.  Tools in MCP are callable functions that an LLM can invoke to perform actions or retrieve data.  Unlike traditional APIs, agents are non‑deterministic: they may choose different solution paths on successive runs, and they have limited context windows.  Writing tools for agents therefore requires designing for cognitive friendliness rather than developer convenience.  For example, a typical list_contacts API that returns all contacts may overwhelm an agent with hundreds of tokens, whereas a search‑based search_contacts tool lets the agent narrow the results ￼.  Effective tools help agents make progress quickly, reduce token usage, and avoid confusion ￼.

Systematic Tool Development Process

Rapid prototyping

Begin with a minimal prototype of your tool and test it in a local MCP server.  Rapid prototyping encourages experimentation; for instance, a schedule_meeting tool can wrap multiple operations (finding availability, creating the meeting, sending invitations) so agents call one tool instead of orchestrating multiple low‑level calls ￼.  Evaluate prototypes with real data and iterate based on user feedback. ￼

Build an evaluation framework

After prototyping, build an evaluation framework to measure how well agents use your tool.  Create realistic tasks that stress the tool in contexts similar to production usage.  Evaluation prompts should reflect complex workflows (e.g., “Customer ID 9182 reported duplicate charges – find logs and determine if other customers were affected”), rather than trivial API queries ￼.  Use test agents to run these tasks programmatically via direct MCP calls and collect structured responses and reasoning.  Evaluate metrics such as tool call frequency, token consumption and task completion time to identify improvements ￼.

Agent collaboration and optimization

Use LLMs themselves to analyze tool logs and optimize tool descriptions.  For example, let Claude analyze usage logs, identify failure patterns and suggest refinements ￼.  After implementing improvements, run the evaluation again to measure the impact.  This feedback loop—prototype → evaluation → optimization—ensures that tools remain aligned with agent capabilities and real‑world tasks.

Core Design Principles

1. Choose the right abstraction level

Tools should encapsulate complete tasks rather than exposing low‑level functions.  Avoid mapping each API endpoint to a separate tool.  For example, rather than providing list_users, list_events and create_event as separate tools, implement a higher‑level schedule_event tool that finds participants’ availability and books the meeting ￼.  This abstraction reduces the number of tool calls and provides a more natural interface for agents ￼.

2. Smart namespacing

When many tools exist across multiple services, namespacing helps agents pick the right tool.  Prefix tool names to reflect the service and resource (e.g., asana_search_projects, jira_search_issues) ￼; this grouping reduces ambiguity and improves discoverability ￼.

3. Return meaningful context

Agents thrive on high‑signal information.  Avoid returning raw technical identifiers (UUIDs, MIME types) that require extra parsing.  Instead, provide human‑readable fields (e.g., name, role, avatar_url, status) ￼.  Tools can still offer a “detailed” mode for IDs and metadata when needed, but the default response should be concise ￼.  Provide both concise and detailed modes via an enum parameter so the agent can choose the level of detail ￼.

4. Optimize token efficiency

Agents have limited context windows, so responses must be as concise as possible.  Support pagination, filtering and range selection for large datasets ￼.  Provide sensible default limits (e.g., 25 k tokens in Claude Code) and allow the agent to request additional pages when necessary ￼.  Truncate long responses intelligently and return clear error messages that suggest more token‑efficient strategies (e.g., using filters or pagination) ￼.

5. Write precise tool descriptions

Tool descriptions are loaded into the agent’s context; they are the agent’s only window into the tool’s purpose.  Descriptions must clearly explain what the tool does, list each parameter with its meaning, note whether the parameter is optional or required, and provide usage examples ￼.  Avoid jargon and ambiguous names, and include examples of valid input formats and expected outputs ￼.  Even small refinements to tool descriptions can yield significant improvements in tool usage ￼.

Practical Guidelines for Implementing Tools

General configuration and behavior
	•	Sensible defaults: Provide reasonable default values for all environment variables so users can run your tool without manual configuration ￼.
	•	Dynamic versioning: Read the tool’s version from package metadata (e.g., package.json) and include it in the tool description; avoid hard‑coding version strings ￼.
	•	Tool & parameter descriptions: Write descriptive, human‑friendly titles and parameter names; clearly indicate which parameters are required and what the defaults are ￼.
	•	Parameter parsing: Accept variations in parameter names (e.g., treat path as project_path) and coerce types gracefully to accommodate agent inconsistencies ￼.
	•	Error handling: When an error occurs, return helpful messages that explain the problem and potential remedies, rather than crashing or failing silently ￼.
	•	Output control: Avoid printing to standard output during normal operation; log events to files instead to prevent noise in MCP clients ￼.
	•	Info command: Implement an info command that reports the tool version, native dependency status and any configuration issues, making troubleshooting easier ￼.

Logging and diagnostics

Use a robust logging framework such as Pino with sensible defaults.  Write logs to a file in the user’s log directory and allow customization via environment variables ￼.  The logging implementation should automatically create missing directories, provide configurable log levels, optionally log to the console, and flush logs before exit ￼.

Code quality and build
	•	Keep dependencies up to date and ensure the code passes static analysis and linting checks ￼.
	•	Avoid large source files; aim for modules under 300–500 lines for readability ￼.
	•	Always run tools using the compiled JavaScript (e.g., dist/ folder) and include appropriate shebang lines for CLI executables ￼.
	•	Ensure the published npm package contains only essential files: compiled code, native components, README and license ￼.

Testing and release
	•	Use a modern test framework (e.g., Vitest) for unit tests, and include comprehensive end‑to‑end tests that simulate complete agent workflows ￼.  Validate CLI behavior and error handling in these tests ￼.
	•	If your tool includes a native binary, maintain a separate test suite for the native component and ensure cross‑platform compatibility (e.g., universal macOS binaries) ￼.
	•	Before releasing, run a scripted checklist: confirm you are on the correct branch, check for uncommitted changes, synchronize with the main branch, verify version consistency, run security audits, compile code and run all tests ￼.  Publish beta releases first for external testing ￼.

Native binary considerations

If your tool wraps a native executable, ensure the native binary is universal (Apple Silicon & Intel), uses minimal optimization flags and contains a helpful --help command ￼.  Provide a custom path environment variable for specifying an alternate binary location ￼, and synchronise version numbers between the native binary and the TypeScript package ￼.  Use a JSON‑based communication mode to return structured output and debug information ￼.

Designing High‑Quality Tool Responses

Namespacing and tool selection

Agents may have access to dozens of MCP servers and hundreds of tools.  To prevent confusion, group tools under clear namespaces (service and resource), such as asana_search or jira_projects_search ￼ ￼.  Provide only the tools needed for the current conversation to avoid overwhelming the agent; tool loadout should be curated carefully to minimize token usage and confusion ￼.

Returning meaningful context

Focus on relevant, human‑interpretable fields.  For example, return name, role and status rather than cryptic identifiers ￼.  Offer options to switch between “concise” (summary information) and “detailed” (includes IDs, metadata) response formats so the agent can balance context richness against token usage ￼.  Provide examples of both modes in the tool description.  Use enumeration types (Enum) for response format options and document each option clearly ￼.

Token efficiency and truncation

Implement pagination, range queries, and sensible default limits to prevent large responses ￼.  Use truncation where necessary and include clear error messages instructing the agent how to retrieve more data or refine the query ￼.  Encourage agents to use many small, targeted searches rather than one broad search when retrieving information ￼.

Prompt‑engineering tool descriptions

Write tool descriptions as if you are onboarding a new colleague.  Explicitly document specialized query formats, domain‑specific terminology and relationships between resources ￼.  Provide usage examples and clarify input and output expectations.  Use strict schemas to enforce correct input, but design your tool to be forgiving at runtime (e.g., accept path when project_path is defined) ￼.

Evaluating and Optimizing Tools

Generating evaluation tasks

Create evaluation prompts based on real‑world scenarios; avoid trivial tasks that test only simple API calls ￼.  Each prompt should have a verifiable outcome so you can measure success precisely ￼.  Use variations in phrasing and inputs to ensure the tool generalizes across similar but distinct tasks.

Running evaluations

Execute evaluations programmatically using direct LLM API calls in a simple loop: call the model, let it call the tool, verify the result, then iterate ￼.  Instruct the agent to output reasoning and feedback alongside the structured responses ￼.  This chain‑of‑thought output helps identify where the agent gets confused or misuses the tool ￼.

Analyzing results and collaborating with agents

Review transcripts of tool calls, agent reasoning and feedback to identify patterns of misuse.  Agents may omit problems in their direct feedback, so careful transcript analysis is crucial ￼.  You can even feed these transcripts back into an LLM (such as Claude Code) to automatically suggest improvements and optimize tool descriptions ￼.

Performance metrics

In addition to accuracy, track: (1) tool call frequency and efficiency, (2) token consumption, (3) task completion time and (4) error rates ￼.  Aim to minimize unnecessary tool calls and tokens while maximizing successful task completion.

Common Pitfalls and How to Avoid Them
	•	One tool per endpoint: Avoid creating a tool for every API endpoint; instead design higher‑level tools that accomplish complete tasks ￼.
	•	Returning low‑level details: Do not return technical identifiers or large objects; provide high‑signal information and allow agents to request detailed context only when needed ￼.
	•	Vague or overlapping names: Use precise namespacing and descriptive names so agents don’t confuse similar tools ￼ ￼.
	•	Neglecting tool descriptions: Poorly written descriptions hinder the agent’s ability to use your tool; invest time in writing clear, comprehensive descriptions ￼ ￼.
	•	Not testing actual workflows: Evaluate tools with realistic tasks; synthetic tests may miss critical edge cases ￼.

Performance Optimization Tips
	•	Consolidate functionality: Merge commonly chained operations (like searching and filtering) into single tools to reduce the number of agent calls ￼.
	•	Smart defaults: Set reasonable default parameter values and return modes to simplify agent usage ￼.
	•	Clear error messages: Provide actionable error messages and suggestions for how to correct the input ￼ ￼.
	•	Support multiple response modes: Offer concise and detailed response formats to optimize context usage ￼.
	•	Context management: Return only relevant information and encourage targeted queries; use pagination, filters and truncation to minimize token usage ￼.

Conclusion and Future Outlook

The MCP ecosystem is rapidly evolving, and tool quality directly determines how effectively agents can perform tasks.  Creating effective MCP tools requires a shift from traditional API design to agent‑centric thinking: tools must encapsulate higher‑level tasks, provide meaningful context, conserve tokens and offer clear guidance.  A systematic, evaluation‑driven development cycle—prototype, evaluate, optimize—combined with robust configuration, logging, testing and release practices ensures reliability and maintainability.  As agents become more capable and new tool patterns emerge, these best practices will evolve, but the principles of clarity, efficiency and agent‑friendliness will remain constant ￼ ￼.