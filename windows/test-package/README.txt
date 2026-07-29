MyBrewFolio Sync Microsoft Store package test

1. Uninstall any directly installed MyBrewFolio Sync MSI first. This prevents an old installation or old application data from masking a packaging problem.
2. Extract this ZIP file completely.
3. Open PowerShell in the extracted directory.
4. Run:  PowerShell -ExecutionPolicy Bypass -File .\Install-TestPackage.ps1
5. Confirm that the interface opens both with and without internet access.
6. Test sign-in, OAuth return, tray behavior, window reopening and a manual sync.
7. Close the app and run:  PowerShell -ExecutionPolicy Bypass -File .\Uninstall-TestPackage.ps1

If the Microsoft Edge error page appears, do not submit the Partner Center package. Locate the log with:

Get-ChildItem "$env:LOCALAPPDATA\Packages" -Filter startup-diagnostics.log -Recurse -ErrorAction SilentlyContinue

Attach startup-diagnostics.log to the issue report. The file contains app, Windows and WebView2 versions plus startup readiness only. It never contains tokens, local addresses or synchronized data.
