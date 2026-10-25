# Interface assets

Flick embeds its interface assets in the executable so icons and typography work independently of the working directory or installed system fonts.

- `fonts/Inter.ttf`: Inter variable font from the [Google Fonts Inter package](https://github.com/google/fonts/tree/main/ofl/inter), licensed under the SIL Open Font License in `fonts/OFL.txt`.
- `icons/*.svg`: Stroke-rounded icons from the free [Hugeicons](https://hugeicons.com) set, converted from the [`@hugeicons/core-free-icons` 4.3.5](https://www.npmjs.com/package/@hugeicons/core-free-icons) package. Files keep Flick's semantic names (for example, `rotate-ccw.svg` is `GoBackward10SecIcon`). The MIT license is retained in `icons/LICENSE`. `icons/picture-in-picture.svg` was drawn for Flick in the same stroke style. Flick has no project license yet, so this icon has no license grant until one is chosen.
- `../packaging/macos/Flick-icon.svg` and its rendered `Flick-icon-1024.png` are the macOS app icon, drawn for Flick around the Hugeicons play glyph from `icons/play.svg`. The app bundle includes the Hugeicons license. These files are not embedded in the executable.

Interface colors, metrics, and fonts come from the selected macOS, Windows, or GNOME style in `src/theme.rs`. Video content retains its original colors.
