# Error evidence

The probe quotes evidence, with a role that states what its syntax establishes:

| Role | Meaning |
|---|---|
| Message definition | A Python literal assignment or catalogue entry; no emission inferred. |
| Occurrence candidate | A textual match; its role or runtime binding is unresolved. |
| Documentation occurrence | A Python standalone string/docstring; no emission proved. |
| Local raise syntax | A Python `raise` at the selected source line; message origin remains a candidate. |
| Conversion/rethrow candidate | A handler with a matching caught type and raise syntax; source branches remain visible. |
| Dependency boundary candidate | A name-matching import; binding unresolved and external emission not inspected. |

The Python handler scan uses prompt/type words, at most 80 non-test candidate
files, one best handler, up to 56 handler lines and six raise excerpts. It is
not exhaustive. Other languages retain textual candidates. Import statements
are syntax evidence, not proof of an unshadowed runtime binding.

Python occurrence membership uses actual AST function spans, so a global message
is not assigned to the preceding function. A handler's local raise excerpts exclude
nested function, async function and class scopes. Other languages use a nearby
function with scope explicitly unresolved.

Name-only callers and tests are explicitly unresolved. Direct matches precede
secondary context; at most two bounded tiers are quoted. Secondary references
are labelled independent context, with no composed path, and are skipped when
the origin name has multiple definitions. Directory proximity does not establish
receiver identity; every lexical call header marks scope/binding uncertainty.
