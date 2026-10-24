# Interface assets

Flick embeds its interface assets in the executable so icons and typography work independently of the working directory or installed system fonts.

- `fonts/Inter.ttf`: Inter variable font from the [Google Fonts Inter package](https://github.com/google/fonts/tree/main/ofl/inter), licensed under the SIL Open Font License in `fonts/OFL.txt`.
- `icons/*.svg`: Stroke-rounded icons from the free [Hugeicons](https://hugeicons.com) set, converted from the [`@hugeicons/core-free-icons` 4.3.5](https://www.npmjs.com/package/@hugeicons/core-free-icons) package. Files keep Flick's semantic names (for example, `rotate-ccw.svg` is `GoBackward10SecIcon`). The MIT license is retained in `icons/LICENSE`. `icons/picture-in-picture.svg` was drawn for Flick in the same stroke style and is covered by Flick's own license.

Interface colors, metrics, and fonts come from the selected macOS, Windows, or GNOME style in `src/theme.rs`. Video content retains its original colors.
