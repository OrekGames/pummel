# Dynamic Scenarios

Dynamic scenarios let a run bind fixture rows to virtual-user iterations, render
request fields from those rows and extracted response values, and branch later
steps from runtime state. This is the initial v1 design. It keeps the existing
custom `{{...}}` template engine and documents the small JSON-path subset used
by extractors, JSON fixture roots, and data-source row lookups.

## Data Sources

Top-level `[data_sources.<id>]` entries define fixture files available to every
scenario in a config:

```toml
[data_sources.users]
type = "csv"
path = "fixtures/users.csv"
access = "per_vu"
exhaustion = "fail"

[data_sources.users.columns]
age = "integer"
active = "bool"
profile = "json"
```

Supported source types are `csv` and `json`.

- `path` is required. Paths loaded through `Config::from_toml` or
  `Config::from_yaml` resolve relative to the config file directory. String and
  programmatic configs resolve relative to the process cwd unless
  `Config::with_source_dir`, `Config::set_source_dir`, or
  `ConfigBuilder::source_dir` sets a base directory.
- `root` is optional for JSON and selects a JSON object or array. It is invalid
  for CSV.
- `access` is `per_vu`, `sequential`, or `random`.
- `exhaustion` is `fail` or `wrap` for finite row access.
- `seed` makes `random` deterministic by source id, VU id, and iteration.
- `columns` is a CSV-only type map. Columns not listed remain strings.

CSV column types are `string`, `integer`, `number`, `bool`/`boolean`, and
`json`. Empty typed CSV fields become `null` except for `string`, where an empty
field remains an empty string. JSON sources keep their native strings, numbers,
booleans, arrays, objects, and nulls.

## Row Binding

Rows are bound once at the start of a VU iteration and reused by every step in
that iteration.

- `per_vu` uses the zero-based VU id as the row index. With `exhaustion =
  "fail"`, validation rejects configs that reference a per-VU source with fewer
  rows than the scenario's maximum configured VU count.
- `sequential` uses one shared cursor per scenario run. Each VU iteration gets
  the next row, then all steps in that iteration reuse it.
- `random` selects a row per source/VU/iteration. When `seed` is set, selection
  is deterministic and independent of task scheduling.

## Templates

Request URLs, headers, text bodies, and JSON bodies can include template
expressions:

```toml
url = "https://api.example.com/profile/{{data.users.username}}"
json = '{"username":"{{data.users.username}}","age":{{data.users.age}}}'

[steps.profile.headers]
Authorization = "Bearer {{token}}"
```

Supported expressions are:

- Extracted variables by `{{name}}` or `{{var.name}}`.
- Built-ins: `{{vu.id}}`, `{{scenario.id}}`, `{{step.id}}`,
  `{{iteration}}`, `{{uuid}}`, `{{random.u64}}`, and
  `{{random.int:min:max}}`.
- Data-source values by `{{data.<source>.<path>}}`.

Stringification is intentionally simple: strings render raw, scalar values
render as JSON literals (`31`, `true`, `null`), and arrays/objects render as
compact JSON.

## JSON-Path Subset

Pummel supports only this JSON-path subset:

- `$`
- `$.field`
- Nested dot fields, such as `$.user.profile.email`
- Array indexes, such as `$.items[0].id` or `$[0]`

Wildcards, filters, recursive descent, and quoted key syntax are not supported.
For data templates, the path after `data.<source>.` may omit the leading `$`,
so `{{data.users.username}}` is equivalent to selecting `$.username` from the
bound row.

## Extractors And Branches

Extractors populate VU-local variables from responses. JSON extractors use the
same JSON-path subset. Regex extractors store capture group 1 when present, or
the whole match otherwise.

Branches run before a step sends a request. Supported operators are:

- `exists`
- `equals`
- `not_equals`
- `greater_than`
- `greater_than_or_equal`
- `less_than`
- `less_than_or_equal`
- `matches_regex`

Numeric branch operators parse the actual value and configured value as finite
numbers. Regex branches match against the same stringified value used by
templates.

## Validation And Linting

`Config::dynamic_lint_report()` loads fixture files and validates dynamic
references without generating load. `Config::validate()` uses the same analysis.
The analysis rejects malformed templates, missing data sources, bad fixture
paths or typed CSV values, invalid JSON paths, missing data paths, unknown
template variables, impossible branch variables, invalid numeric branch values,
and invalid regexes.

## Non-Goals

This v1 does not add scripting, expression evaluation, reusable flow fragments,
weighted flow selection, or a full JSONPath implementation. Step `weight`
remains a launch-priority hint among ready steps; it does not select among
alternative flows or change how often a step runs.
