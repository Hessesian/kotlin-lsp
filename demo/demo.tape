Output kotlin-lsp-demo.gif

Set Shell "bash"
Set FontSize 14
Set Width 1200
Set Height 650
Set Theme "TokyoNight"
Set Padding 16
Set CursorBlink false

# cd into project, open with full helix path (instant typing for long path)
Set TypingSpeed 0ms
Type "cd ~/Work/samples/nowinandroid && /usr/bin/helix feature/bookmarks/impl/src/main/kotlin/com/google/samples/apps/nowinandroid/feature/bookmarks/impl/BookmarksViewModel.kt"
Enter
Set TypingSpeed 70ms

# Wait for Helix + kotlin-lsp to boot and index (progress spinner shows)
Sleep 3500ms

# Scene 1: go-to-definition — jump from NewsFeedUiState reference to its file
Type "/NewsFeedUiState>"
Enter
Sleep 300ms
Escape
Type "gd"
Sleep 2500ms

# Scene 2: hover on the sealed interface definition
Type "K"
Sleep 2000ms
Escape
Sleep 200ms

# Scene 3: jump back to BookmarksViewModel, dot-complete on viewModelScope
Ctrl+o
Sleep 500ms
Type "/viewModelScope.launch"
Enter
Sleep 200ms
Escape
Type "ea"
Sleep 150ms
Type "."
Sleep 1500ms
Escape
