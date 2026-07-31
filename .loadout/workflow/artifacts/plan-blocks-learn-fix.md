| type | mdx tag | placement | key data fields | description |
| --- | --- | --- | --- | --- |
| `annotated-code` | `<AnnotatedCode>` | block | `filename?`, `language?`, `code`, `annotations?` | A line-numbered code walkthrough whose line ranges carry anchored explanatory notes. |
| `api-endpoint` | `<Endpoint>` | block | `method`, `path`, `summary?`, `description?`, `auth?`, `deprecated?`, `change?`, `params?`, `request?`, `responses?` | A Swagger-style API endpoint reference: a colored method pill + path, collapsed by default, expanding to params, request body, and per-status response examples. |
| `callout` | `<Callout>` | block | `tone?`, `body` | An emphasized note with a tone (info/decision/risk/warning/success) and a markdown body. |
| `checklist` | `<Checklist>` | block | `items` | A list of toggleable items, each with a label and an optional note. |
| `code` | `<Code>` | block | `code`, `language?`, `filename?`, `caption?`, `maxLines?` | A single syntax-highlighted code snippet (Notion-style: one border, hover language switcher + copy, collapse-to-N lines). Put several in a `tabs` block for a file rail. |
| `code-tabs` | `<CodeTabs>` | block | `tabs` | A vertical file tab rail of syntax-highlighted code snippets, one tab per file. Deprecated: prefer a `tabs` block with `code` children. |
| `columns` | `<Columns>` | block | `columns` | A multi-column side-by-side layout container; each column holds its own list of blocks. Ideal for before/after or current/target comparisons. |
| `custom-html` | `<HtmlBlock>` | block | `html`, `css?`, `caption?` | An author-supplied HTML (with optional CSS) fragment rendered in a sandboxed iframe, with inline source editing. |
| `data-model` | `<DataModel>` | block | `entities`, `relations?` | A schema modeling / ERD / dbdiagram-style data model: entity cards with typed fields (PK/FK/nullable flags) and interactive foreign-key relations. |
| `diagram` | `<Diagram>` | block | `html?`, `css?`, `caption?`, `frame?`, `nodes?`, `edges?`, `notes?` | A flexible inline architecture/code diagram. Prefer html/css with SVG or semantic HTML for polished two-dimensional layouts; use .diagram-* primitives and --wf-* tokens for theme/sketch compatibility. Set frame to show for standalone artifacts that need containment, hide when surrounding docs/canvas chrome already supplies the boundary. Legacy nodes/edges are only for simple previews. |
| `diff` | `<Diff>` | block | `filename?`, `language?`, `before`, `after`, `mode?`, `annotations?` | A GitHub-style before/after line diff for a file, with unified or split view and added/removed line highlighting. |
| `file-tree` | `<FileTree>` | block | `title?`, `entries` | A VS Code / GitHub-explorer file and change tree derived from slash-delimited paths, with per-file change badges (added/modified/removed/renamed), notes, and code snippets. |
| `json-explorer` | `<Json>` | block | `title?`, `json`, `collapsedDepth?` | A collapsible browser-devtools / Postman-style JSON tree with type-colored values and expand/collapse. |
| `mermaid` | `<Mermaid>` | block | `source`, `caption?` | A Mermaid diagram for cases where textual sequence or flowchart grammar is clearer than a spatial layout; not the default for architecture maps. |
| `openapi-spec` | `<OpenApi>` | block | `spec`, `title?` | A whole-document API specification / Redoc / Swagger-UI-style API reference rendered from a complete OpenAPI 3 / Swagger 2 spec (JSON). |
| `question-form` | `<QuestionForm>` | block | `questions`, `submitLabel?` | An interactive respondent-facing form block for open questions, single-choice or multi-choice option rows, freeform answers, recommended options, and optional wireframe/diagram previews. |
| `table` | `<Table>` | block | `columns`, `rows`, `density?` | A simple grid with header columns and string rows for comparisons, parameters, or structured lists. |
| `tabs` | `<TabsBlock>` | block+inline | `tabs`, `orientation?` | A top or side tab container; each tab holds its own list of blocks. |
| `visual-questions` | `<VisualQuestions>` | block | `questions`, `submitLabel?` | A visual-intake question block with the same editable question/option shape as question-form. Deprecated: prefer `question-form`. |
| `wireframe` | `<WireframeBlock>` | block | `surface`, `renderMode?`, `caption?`, `frame?`, `skeleton?`, `html?`, `css?`, `screen?` | A sketch wireframe of one screen built from kit primitives (or an HTML mockup), rendered in a chosen surface (desktop/mobile/popover/panel/browser). Set frame to show for standalone screens and recap comparisons, hide only when surrounding chrome already supplies the boundary. |

## Block headings

Blocks do not take a `title`. To give a block a heading, place a `rich-text` block whose markdown is a `###` (h3) heading directly above the block. Those headings are real, inline-editable, and appear in the document outline — unlike the legacy block `title` field, which renders as a small muted label and cannot be edited in place. This includes the bottom Open Questions form: put an `### Open Questions` heading above the `question-form` block rather than titling it. The `title` field still renders for older plans, but do not set it on new blocks.

## Authoring rules

**Diagrams**: use `diagram` blocks with `data.html`/`data.css`. Use renderer-owned classes: `.diagram-panel`, `.diagram-card`, `.diagram-node`, `.diagram-box`, `.diagram-pill`, `.diagram-muted`, plus `data-rough`. Use `--wf-*` CSS tokens for color; never set custom fonts or hard-code hex/rgb/hsl values. Legacy nodes/edges only for tiny previews or true step-by-step flows. Use `mermaid` only when textual grammar is materially clearer than a spatial layout.

**Wireframes**: set `data.html` to a semantic HTML fragment; pick a surface (desktop/mobile/popover/panel/browser). The renderer owns theme, footprint/aspect, Excalifont, and rough.js sketch overlay. Use `--wf-*` CSS tokens for any custom color (never hex). Prototype screens use semantic HTML with `data-goto` attributes for navigation.

**Visual frames**: `wireframe` and `diagram` data accept `frame: "auto" | "show" | "hide"`. Leave it unset/`auto` when the host context should decide: Plan and recap surfaces default to framed; docs default to unframed. Use `frame: "show"` for standalone product screens, before/after recap comparisons, screenshot-like artifacts, and visuals that need containment from surrounding prose. Use `frame: "hide"` when a docs page, tab, column, card, canvas artboard, or the visual's own internal chrome already supplies the boundary. Hiding the outer frame must not remove inner padding, meaningful card/field/button borders, or the visual's readable structure.

**Canvas storyboards**: if the user asks for a canvas, storyboard, wireframe, light storyboard, UI flow, screen flow, product flow, mockup, or visual comparison, the primary artifact must be `content.canvas` / `canvas.mdx` with `DesignBoard` artboards containing `Screen` HTML wireframes. Each canvas `Screen` must carry `html` / `data.html`; never author fresh nested kit-tree children such as `<FrameScreen>`, `<Card>`, `<Row>`, or `<Btn>` inside canvas `<Screen>` tags. Kit trees are old-plan compatibility only and often render worse on the pan/zoom canvas than HTML wireframes. Do not use document-body `diagram` blocks for the primary UI story. Use `diagram` only for architecture, data flow, or implementation mechanics below the canvas, and only after the UI storyboard exists.

**Before/After columns**: compose a `columns` block from `<Column>` CHILDREN — never a `columns=` attribute or inline JSON array. Author it as `<Columns><Column label="Before">…child block(s)…</Column><Column label="After">…child block(s)…</Column></Columns>`. Each `<Column>` wraps real nested blocks (e.g. a `Wireframe`); the parser fills in column ids and child-block `data` from that markup, whereas a `columns=` attribute array leaves them missing and FAILS schema validation. For UI state comparisons put one `wireframe` block in each side and label the columns `Before` and `After`; the renderer draws labels as headings and lays narrow surfaces side by side. Never bake Before/After labels inside the wireframe HTML or hand-stack the pair.

**MDX prose and component syntax**: write ordinary top-level prose as normal Markdown; it imports as rich-text automatically. Use `<RichText id="...">…</RichText>` only when prose needs explicit metadata such as `title`, `summary`, or `editable`, or when preserving a referenced block id. Every capitalized block component must be self-closing (`<Diagram id="..." data={{ ... }} />`) or have a matching closing tag around children (`<RichText id="...">…</RichText>`). Never write a bare opening tag like `<RichText ...>` as a paragraph; the MDX parser treats it as unclosed JSX and import fails before the plan can render.

**Code-bearing blocks**: `code`, `annotated-code`, and `diff` are whitespace-sensitive. Prefer the exact MDX form emitted by the authoring examples / source exporter, where multiline code is encoded as JSON string attributes such as `code={"const x =\\n  y"}`. Static template literals are accepted and preserve indentation, but they must be static strings with no `${...}` interpolation.

**File maps**: prefer `annotated-code` blocks (real code + line-anchored notes) grouped in a vertical `tabs` block, one tab per key file. Drop to a plain `code` block only for throwaway snippets with nothing to call out.

**Tabs grouping**: use horizontal `TabsBlock` groups for multiple related diffs so each split diff gets full document width.

**API endpoints**: keep `api-endpoint` and `openapi-spec` blocks in normal single-column document flow. Use `columns` only for an explicit before/after contract comparison.

**renderMode**: leave unset or set to `wireframe` unless a design-only editable mock is required.`

## Authoring examples

Copy a working shape from these complete, valid examples instead of inferring one from the JSON schema. Each is generated from a real block and round-trips through the strict source parser, so every required field is present (tab `id`, nested child `data`, checklist item `id`, question-form question/option `id`, api-endpoint `responses[].status`, non-empty `callout` body). Keep block `id`s unique and edit the content; do not drop required fields.

### `columns`

```mdx
<Columns id="example-columns">
<Column id="example-columns-before" label="Before">

<WireframeBlock id="example-columns-before-wf">
  <Screen surface="panel" caption="Single Save button, no autosave indicator.">
    <Title text="Editor" />
    <Btn label="Save" />
  </Screen>
</WireframeBlock>

</Column>

<Column id="example-columns-after" label="After">

<WireframeBlock id="example-columns-after-wf">
  <Screen
    surface="panel"
    caption="Autosave pill replaces the manual Save button."
  >
    <Title text="Editor" />
    <Pill label="Saved" />
  </Screen>
</WireframeBlock>

</Column>
</Columns>
```

### `tabs`

```mdx
<TabsBlock
  id="example-tabs"
  tabs={[
    {
      id: "example-tab-content",
      label: "plan-content.ts",
      blocks: [
        {
          id: "example-tab-content-note",
          type: "rich-text",
          data: {
            markdown: "Added per-block salvage to the parse path.",
          },
        },
      ],
    },
    {
      id: "example-tab-mdx",
      label: "plan-mdx.ts",
      blocks: [
        {
          id: "example-tab-mdx-note",
          type: "rich-text",
          data: {
            markdown: "Threads the `salvageInvalidBlocks` flag.",
          },
        },
      ],
    },
  ]}
/>
```

### `api-endpoint`

```mdx
<Endpoint id="example-api-endpoint" method="POST" path="/v1/messages" summary="Create a message" auth="Bearer token" params={[
  {
    "name": "idempotency-key",
    "in": "header",
    "type": "string"
  },
  {
    "name": "model",
    "in": "body",
    "type": "string",
    "required": true,
    "description": "Model id."
  }
]} request={{
  "contentType": "application/json",
  "example": "{ \"model\": \"claude-3\", \"messages\": [] }"
}} responses={[
  {
    "status": "200",
    "description": "OK",
    "example": "{ \"id\": \"msg_1\" }"
  },
  {
    "status": "429",
    "description": "Rate limited"
  }
]}>

Creates a message and returns the assistant response.

</Endpoint>
```

### `data-model`

```mdx
<DataModel
  id="example-data-model"
  entities={[
    {
      id: "plans",
      name: "plans",
      fields: [
        {
          name: "id",
          type: "uuid",
          pk: true,
        },
        {
          name: "owner_id",
          type: "uuid",
          fk: "users.id",
        },
        {
          name: "content",
          type: "jsonb",
        },
      ],
    },
    {
      id: "users",
      name: "users",
      fields: [
        {
          name: "id",
          type: "uuid",
          pk: true,
        },
      ],
    },
  ]}
  relations={[
    {
      from: "plans",
      to: "users",
      kind: "1-n",
    },
  ]}
/>
```

### `annotated-code`

```mdx
<AnnotatedCode
  id="example-annotated-code"
  filename="server/plan-content.ts"
  language="ts"
  code={
    "export function normalizePlanContent(\n  content: PlanContentInput | undefined,\n  options: { salvageInvalidBlocks?: boolean } = {},\n) {\n  const migrated = migratePlanContent(content);\n  return sanitizePlanContent(planContentSchema.parse(migrated));\n}"
  }
  annotations={[
    {
      lines: "4",
      label: "Salvage flag",
      note: "Recaps pass `salvageInvalidBlocks: true`; plans stay strict.",
    },
    {
      lines: "5-6",
      note: "Migrate legacy shapes before validating.",
    },
  ]}
/>
```

### `diff`

```mdx
<Diff
  id="example-diff"
  filename="server/plan-content.ts"
  language="ts"
  mode="split"
  before={"function parse(value) {\n  return JSON.parse(value);\n}\n"}
  after={
    "function parse(value) {\n  try {\n    return JSON.parse(value);\n  } catch {\n    return null;\n  }\n}\n"
  }
  annotations={[
    {
      side: "after",
      lines: "2-5",
      label: "Fail closed",
      note: "Return null instead of throwing on malformed JSON.",
    },
  ]}
/>
```

### `file-tree`

```mdx
<FileTree
  id="example-file-tree"
  title="Files touched"
  entries={[
    {
      path: "server/plan-content.ts",
      change: "modified",
      note: "Added per-block salvage.",
    },
    {
      path: "server/plan-block-examples.ts",
      change: "added",
      note: "New canonical authoring examples.",
    },
  ]}
/>
```

### `diagram`

```mdx
<Diagram id="example-diagram" caption="Recap import flow.">

```html
<div class="diagram-panel" data-rough><div class="diagram-node">Diff</div><div class="diagram-node">Recap blocks</div><div class="diagram-node">Published recap</div></div>
```

```css
.diagram-panel { display: flex; gap: 12px; }
```

</Diagram>
```

### `wireframe`

```mdx
<WireframeBlock id="example-wireframe">
  <Screen surface="desktop" caption="Recap detail with a Files-touched rail.">
    <Row>
      <Sidebar>
        <NavItem label="Summary" />
        <NavItem label="Files touched" active />
      </Sidebar>
      <Main>
        <Title text="Visual recap" />
        <Lines n={3} />
      </Main>
    </Row>
  </Screen>
</WireframeBlock>
```

### `code`

```mdx
<Code
  id="example-code"
  filename="server/plan-block-examples.ts"
  language="ts"
  caption="Serialize one canonical block to its MDX authoring form."
  code={"const mdx = await serializeBlockToMdx(EXAMPLE_BLOCKS.code);\n"}
/>
```

### `callout`

```mdx
<Callout id="example-callout" tone="info">

This recap is informational; reviewers still inspect the diff.

</Callout>
```

### `checklist`

```mdx
<Checklist
  id="example-checklist"
  items={[
    {
      id: "verify-local-check",
      label: "Run `plan local check` before serving",
      checked: true,
    },
    {
      id: "open-in-chromium",
      label: "Open local-files plans in Chrome/Chromium",
      note: "Safari can block HTTPS pages from reading an HTTP localhost bridge.",
    },
  ]}
/>
```

### `question-form`

```mdx
<QuestionForm
  id="example-question-form"
  questions={[
    {
      id: "local-mode-browser",
      title: "Which browser should local reviewers use?",
      subtitle: "Choose the default recommendation for local-files mode.",
      mode: "single",
      options: [
        {
          id: "chromium",
          label: "Chrome / Chromium",
          detail: "Best support for hosted HTTPS pages reading localhost.",
          recommended: true,
        },
        {
          id: "local-plan-app",
          label: "Local Plan app",
          detail: "Use when the Plan app is running on localhost too.",
        },
      ],
      required: true,
    },
    {
      id: "handoff-notes",
      title: "What should the agent preserve?",
      mode: "freeform",
      placeholder: "Add constraints or review notes...",
    },
  ]}
  submitLabel="Send answers"
/>
```

### `rich-text`

```mdx
### Summary

Adds per-block salvage so one bad block never blanks a recap.
```

### `json-explorer`

```mdx
<Json
  id="example-json-explorer"
  title="Recap payload"
  json={'{\n  "ok": true,\n  "blocks": 4,\n  "salvaged": 0\n}'}
  collapsedDepth={1}
/>
```

### `table`

```mdx
<Table
  id="example-table"
  columns={["Field", "Type", "Note"]}
  rows={[
    ["id", "uuid", "primary key"],
    ["content", "jsonb", "normalized plan blocks"],
  ]}
/>
```