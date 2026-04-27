# Filter Expression Engine

**Status:** Implemented
**Location:** `src/tasks/filter/`

## Overview

cmdock-server includes a native Rust implementation of the Taskwarrior filter expression engine. This evaluates filter strings against TaskChampion `Task` objects in-memory, eliminating the need for the `task` CLI binary on the server.

The engine is designed to be **compatible with Taskwarrior filter syntax** for the subset used by the iOS app's view definitions, while documenting known differences and intentional extensions.

### Architecture

```
Filter string (e.g. "status:pending +DUETODAY or +OVERDUE")
    ↓
Tokeniser (parse.rs) → Vec<RawToken>
    ↓
Implicit AND insertion
    ↓
Recursive descent parser (parse.rs) → FilterExpr AST
    ↓
Evaluator (eval.rs) → bool (per task)
```

**Source files:**

| File | Purpose |
|------|---------|
| `filter/mod.rs` | Module root, exports `matches_filter()` |
| `filter/tokens.rs` | `FilterExpr` AST enum and `AttrModifier` enum |
| `filter/parse.rs` | Tokeniser and recursive descent parser |
| `filter/eval.rs` | Evaluator: walks AST against a TaskChampion `Task` |
| `filter/dates.rs` | Named date resolution (`today`, `eow`, `eom`, etc.) |

**Public API:**

```rust
pub fn matches_filter(task: &Task, filter: &str) -> bool
```

---

## Filter Syntax

### Attribute Filters

Format: `attribute:value` or `attribute.modifier:value`

The default comparison (no modifier) uses **approximate matching**:
- **Strings:** left-match (starts-with). `project:Home` matches `Home`, `Home.Kitchen`, `Home.Garden`.
- **Dates:** same calendar day. `due:today` matches any task due today regardless of time.

```
status:pending              # Status is pending
project:PERSONAL.Home       # Project starts with "PERSONAL.Home"
priority:H                  # Priority is High
description:meeting         # Description starts with "meeting"
due:today                   # Due date is today
priority:                   # No priority set (empty value)
```

### Supported Attributes

| Attribute | Type | Notes |
|-----------|------|-------|
| `status` | enum | `pending`, `completed`, `deleted`, `recurring` |
| `project` | string | Hierarchical (dot-separated), left-match by default |
| `priority` | string | `H`, `M`, `L`, or empty |
| `description` | string | Task description text |
| `due` | date | Due date |
| `entry` | date | When the task was created |
| `modified` | date | Last modification time |
| `wait` | date | Task hidden until this date |
| `scheduled` | date | Scheduled start date |
| `tags` | special | Use with `.has`/`.hasnt`/`.any`/`.none` modifiers |

Any unrecognised attribute name is looked up via `task.get_value(name)` as a generic string property.

### Attribute Modifiers

Format: `attribute.modifier:value`

| Modifier | Aliases | String behaviour | Date behaviour |
|----------|---------|-----------------|----------------|
| *(default)* | | Left-match (starts-with) | Same calendar day |
| `is` | `equals` | Exact match (case-insensitive) | Exact timestamp match |
| `isnt` | `not` | Not exact match | Not exact timestamp |
| `before` | `under`, `below` | Lexicographic `<` | Strictly before |
| `after` | `over`, `above` | Lexicographic `>` | Strictly after |
| `by` | | Lexicographic `<=` | On or before |
| `has` | `contains` | Substring match (case-insensitive) | — |
| `hasnt` | | Not substring match | — |
| `startswith` | `left` | Starts with (case-insensitive) | — |
| `endswith` | `right` | Ends with (case-insensitive) | — |
| `none` | | Attribute has no value | No date set |
| `any` | | Attribute has any value | Has a date |

```
project.is:Home             # Exact match — does NOT match Home.Kitchen
project.startswith:PERSONAL # Starts with PERSONAL
due.before:today            # Overdue (due date before today)
due.after:eow               # Due after end of this week
due.by:friday               # Due on or before Friday
description.has:meeting     # Description contains "meeting"
priority.any:               # Has any priority set
priority.none:              # No priority assigned
```

### Tag Filters

Format: `+tagname` (has tag) or `-tagname` (does not have tag)

```
+shopping                   # Task has the "shopping" tag
-work                       # Task does NOT have the "work" tag
+OVERDUE                    # Virtual tag: task is overdue
```

Tags are case-sensitive for user tags. All-uppercase tags are virtual (synthetic) tags.

### Virtual Tags

Virtual tags are computed from task properties at evaluation time. They are never stored — the engine evaluates them on the fly.

#### Provided by TaskChampion (8 tags)

These are evaluated by TaskChampion's `task.has_tag()` method:

| Tag | Condition |
|-----|-----------|
| `PENDING` | `status == pending` |
| `COMPLETED` | `status == completed` |
| `DELETED` | `status == deleted` |
| `WAITING` | Has `wait` date in the future and status is pending |
| `ACTIVE` | Has been started (`start` property exists) |
| `BLOCKED` | Has unresolved dependencies |
| `UNBLOCKED` | Not blocked |
| `BLOCKING` | Other tasks depend on this one |

#### Implemented by our engine (14 tags)

These extend TaskChampion's built-in set. "Actionable" below means status is neither `completed` nor `deleted`.

| Tag | Condition |
|-----|-----------|
| `OVERDUE` | Actionable and due date is before today |
| `DUETODAY` / `TODAY` | Actionable and due date is today |
| `DUE` | Actionable and due date is within 7 days (including overdue) |
| `TOMORROW` | Actionable and due date is tomorrow |
| `YESTERDAY` | Actionable and due date was yesterday |
| `WEEK` | Actionable and due date falls within the current week (Mon–Sun) |
| `MONTH` | Actionable and due date falls within the current calendar month |
| `YEAR` | Actionable and due date falls within the current calendar year |
| `READY` | Pending, not blocked, not waiting, and not scheduled in the future |
| `TAGGED` | Has at least one user-defined tag |
| `ANNOTATED` | Has at least one annotation |
| `PROJECT` | Has a project assigned |
| `PRIORITY` | Has a priority assigned |
| `SCHEDULED` | Has a scheduled date |

### Boolean Operators

| Operator | Precedence | Description |
|----------|-----------|-------------|
| `or` | Low | Logical OR |
| `and` | High | Logical AND |
| `not` / `!` | Highest | Logical NOT (unary) |
| `(` `)` | — | Grouping |

**Implicit AND:** Adjacent filter terms without an explicit operator are joined with AND.

```
status:pending +shopping
```
is equivalent to:
```
status:pending and +shopping
```

**Precedence matters with `or`:**

```
status:pending +DUETODAY or +OVERDUE
```
parses as:
```
(status:pending AND +DUETODAY) OR +OVERDUE
```

Use parentheses for the intended grouping:
```
status:pending (+DUETODAY or +OVERDUE)
```
parses as:
```
status:pending AND (+DUETODAY OR +OVERDUE)
```

### Bare Words

A bare word (no `+`, `-`, `:`, or recognised operator) is treated as a substring search on `description`:

```
meeting
```
is equivalent to:
```
description.has:meeting
```

### Named Dates

Used in date attribute filters like `due.before:eow`.

#### Current period

| Name | Resolves to |
|------|-------------|
| `now` | Current date and time |
| `today` / `sod` | Today at 00:00:00 UTC |
| `yesterday` | Yesterday at 00:00:00 UTC |
| `tomorrow` | Tomorrow at 00:00:00 UTC |
| `eod` | Today at 23:59:59 UTC |

#### Week boundaries (Monday = start of week)

| Name | Resolves to |
|------|-------------|
| `sow` | Start of current week (Monday 00:00:00) |
| `eow` | End of current week (Sunday 23:59:59) |
| `sonw` | Start of next week |
| `eonw` | End of next week |
| `sopw` | Start of previous week |
| `eopw` | End of previous week |

#### Month boundaries

| Name | Resolves to |
|------|-------------|
| `som` | First day of current month |
| `eom` | Last day of current month |
| `sonm` | First day of next month |
| `eonm` | Last day of next month |
| `sopm` | First day of previous month |
| `eopm` | Last day of previous month |

#### Year boundaries

| Name | Resolves to |
|------|-------------|
| `soy` | January 1st of current year |
| `eoy` | December 31st of current year |

#### Day names

`monday`/`mon`, `tuesday`/`tue`, `wednesday`/`wed`, `thursday`/`thu`, `friday`/`fri`, `saturday`/`sat`, `sunday`/`sun`

Resolves to the **next** occurrence of that weekday at 00:00:00 UTC.

#### Special

| Name | Resolves to |
|------|-------------|
| `later` / `someday` | 9999-12-30 |

### Date Formats

Date values in filters are resolved in this order:

1. **Named date** (e.g., `today`, `eow`, `friday`)
2. **Taskwarrior format** `YYYYMMDDTHHmmssZ` (e.g., `20260328T090000Z`)
3. **ISO format** `YYYY-MM-DD` (e.g., `2026-03-28`)
4. **Epoch seconds** (e.g., `1743120000`)

---

## Differences from Taskwarrior

### Not implemented

These Taskwarrior features are not currently supported:

| Feature | Reason |
|---------|--------|
| `xor` operator | Not used by iOS app views |
| Regex patterns (`/pattern/`) | Can be added if needed |
| `urgency.above:N` filter | Urgency uses stock TW defaults server-side (#77), not filterable yet |
| `limit:N` / `limit:page` | Pagination handled at the API level |
| `depends:ID` filter | Dependency filtering via attribute not implemented |
| `recur:` filter | Recurrence not fully supported yet |
| Duration math (`due.before:now+2d`) | Named dates cover most use cases |
| Work week boundaries (`soww`, `eoww`) | Not needed for current views |
| Quarter boundaries (`soq`, `eoq`) | Not needed for current views |
| Holiday dates (`easter`, `goodfriday`) | Not needed |
| Ordinal dates (`1st`, `15th`) | Not needed for current views |
| `LATEST`, `UDA`, `ORPHAN` virtual tags | Not applicable to server context |
| `PARENT`/`TEMPLATE`/`CHILD`/`INSTANCE` | Recurring task templates not yet supported |

### Behavioural differences

| Behaviour | Taskwarrior | Our engine |
|-----------|-------------|------------|
| String matching | Configurable via `rc.search.case.sensitive` | Always case-insensitive |
| Date timezone | Uses local timezone | All dates are UTC |
| `DUE` virtual tag window | Configurable via `rc.due` (default 7 days) | Fixed at 7 days |
| Week start day | Configurable via `rc.weekstart` | Always Monday |
| Attribute `=` for dates | Same calendar day (local) | Same calendar day (UTC) |

---

## Common View Filters

These are representative builtin and client-used filter expressions. All are fully supported:

```
# Due Soon — pending tasks due within 7 days, excluding blocked and waiting work
status:pending -BLOCKED -WAITING due.before:7d

# Shopping — shopping tasks grouped by store tags
status:pending project:PERSONAL.Home +shopping

# Action — high-priority pending tasks, excluding blocked and waiting work
status:pending -BLOCKED -WAITING priority:H

# Personal — all personal project tasks
status:pending project:PERSONAL

# Work — work project tasks (multiple prefixes)
status:pending (project.startswith:10FIFTEEN or project.startswith:SSRP)

# Health — health-related tasks
status:pending project:PERSONAL.Health

# All — everything pending
status:pending

# Completed recently
status:completed
```

---

## Extension Points

The engine is designed to be extended. Potential additions:

1. **Custom virtual tags** — Server-side tags computed from config (e.g., context-aware tags like `+WORK` that expand to project prefix checks)
2. **Regex support** — Add `/pattern/` syntax for description matching
3. **`urgency` filter** — Filter by computed urgency score
4. **Duration math** — Support `due.before:now+2d` syntax
5. **`limit` support** — Return at most N results from a filter

To add a new virtual tag, add a match arm in `eval_virtual_tag()` in `eval.rs`. To add a new named date, add a match arm in `resolve_named_date()` in `dates.rs`.

---

## Grammar

```
filter     = or_expr
or_expr    = and_expr { "or" and_expr }
and_expr   = unary { "and" unary }
unary      = ["not" | "!"] primary
primary    = "(" or_expr ")"
           | attribute_filter
           | tag_filter
           | bare_word

attribute_filter = name ["." modifier] ":" value
tag_filter       = "+" tag_name | "-" tag_name
bare_word        = <any word> → description.has:<word>

Implicit AND inserted between adjacent primary tokens.
```
