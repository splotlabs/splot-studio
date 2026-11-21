Splot Studio @VERSION@ - UNSIGNED TEST BUILD (@TARGET@)

This is a test package, not a release. It is not code-signed@SIGNING_NOTE@,
so the operating system will warn before the first start. It has not been
tested on a clean machine. See RELEASE-NOTES in the source repository
(docs/release/) for what it does, what it refuses, and the known limitations.

What is inside
@LAYOUT@
Install: unpack this archive anywhere you can write to and keep the files
together; the app starts its engine and FFmpeg from beside itself.
Update: quit the app, replace the folder with the new one.
Uninstall: quit the app and delete the folder. Your list of files and settings
live in a separate per-user folder (Linux ~/.local/share/SplotStudio, macOS
~/Library/Application Support/SplotStudio, Windows %LOCALAPPDATA%\SplotStudio);
delete it too to remove everything. Your videos are never touched.

Output is video only (AV2 in IVF or Annex B .obu); audio, subtitles and other
tracks of a source are not carried over.
