# Error evidence

The probe quotes evidence, with a role that states what its syntax establishes:

| Role | Meaning |
|---|---|
| Message definition | A Python literal assignment or catalogue entry; no emission inferred. |
| Occurrence candidate | A textual match; its role or runtime binding is unresolved. |
| Documentation occurrence | A Python standalone string/docstring; no emission proved. |
| Local raise syntax | A Python `raise` at the selected source line; message origin remains a candidate. |
| Conversion/rethrow candidate | A handler with a matching caught type and raise syntax; source branches remain visible. |
| Dependency boundary | An import for the caught type; the external emission has not been inspected. |

The Python handler scan uses prompt/type words, at most 80 non-test candidate
files, one best handler, up to 56 handler lines and six raise excerpts. It is
not exhaustive. Other languages retain textual candidates. Import statements
are syntax evidence, not proof of an unshadowed runtime binding.

Name-only callers and tests are explicitly unresolved. Only one level of
lexical call context is shown; an unresolved edge is never extended into a
causal chain. Directory proximity does not establish receiver identity.
