# Meadow Theme for VS Code

The Meadow colour scheme, as two themes: **Meadow Dark** and **Meadow Light**.

Each kind of name has a colour of its own, after the way the Meadow REPL
colours what is typed at it:

| What                          | Dark      | Light     |
| ----------------------------- | --------- | --------- |
| Keywords                      | `#B393F4` | `#8A3FB5` |
| Types                         | `#FFB07A` | `#B4541A` |
| Modules and paths             | `#E0A3F5` | `#9A3FB8` |
| Constructors and enum members | `#889FEC` | `#3F5BC4` |
| Functions                     | `#FFE08A` | `#9A6A0E` |
| Variables and parameters      | `#A5B7F2` | `#4A67D6` |
| Numbers                       | `#FFD866` | `#8A5D08` |
| Strings                       | `#A9DC76` | `#1E7A4F` |
| Attributes and lifetimes      | `#E6B66C` | `#8A6A2A` |
| Comments                      | `#8C898D` | `#5C6576` |

Operators and punctuation are left the colour of the text. The window and the
terminal's sixteen colours follow the same palette, with a royal blue accent.

A type, a constructor and a module are all capitalised, and only a language
server knows which is which, so those three are told apart by semantic tokens.
They show best with the [Meadow extension](../vscode) running `meadow lsp`;
the theme is not tied to Meadow, though, and colours any language whose server
sends semantic tokens the same way.

## Install

```sh
editors/vscode-theme/build.sh
code --install-extension editors/vscode-theme/meadow-theme-*.vsix
```

then choose it with **Preferences: Color Theme**. Each release also attaches a
built `.vsix`, and `scripts/install.sh --with-extension` installs it beside the
language extension.

The theme is a separate extension from the language support so that either can
be used without the other.
