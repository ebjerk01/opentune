# Bundled fonts

| Font | Files | Licence | Source |
|---|---|---|---|
| Inter 4.1 | Inter-Regular/Medium/SemiBold.ttf | SIL OFL 1.1 (Inter-OFL.txt) | github.com/rsms/inter (release v4.1, extras/ttf) |
| Space Grotesk 2.0.0 | SpaceGrotesk-Medium/Bold.ttf | SIL OFL 1.1 (SpaceGrotesk-OFL.txt) | github.com/floriankarsten/space-grotesk (release 2.0.0, static) |
| JetBrains Mono 2.304 | JetBrainsMono-Regular/Medium/SemiBold.ttf | SIL OFL 1.1 (JetBrainsMono-OFL.txt) | github.com/JetBrains/JetBrainsMono (release v2.304) |
| Phosphor Icons 2.1.2 | Phosphor.ttf (regular weight) | MIT (Phosphor-MIT.txt) | npm @phosphor-icons/web 2.1.2 |

**Modification:** the Inter, Space Grotesk and JetBrains Mono files were
subset with fontTools to drop the Private Use Area (U+E000–U+F8FF), which
clashed with Phosphor's icon codepoints; everything else (all other
characters, OpenType layout features, names, hinting) is kept. None of these
fonts declares a Reserved Font Name, so the OFL allows the modified files to
keep their names.

```sh
python3 -m fontTools.subset FONT.ttf --unicodes="U+0000-DFFF,U+F900-10FFFF" \
  --layout-features='*' --glyph-names --notdef-outline --name-IDs='*' \
  --name-legacy --name-languages='*' --hinting --legacy-kern
```
