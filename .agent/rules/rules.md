---
trigger: always_on
---

Prefer using native file system tools over terminal commands like cat for reading file contents
unless terminal verification is required.

To search code, use the `rg` command (i.e. ripgrep) instead of `grep`. The `rg` command 
automatically ignores files in the `.gitignore` list, which avoids searching binary 
files and other non-editable files.