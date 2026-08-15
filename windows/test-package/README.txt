MyBrewFolio Sync Microsoft Store package test

1. Uninstall any directly installed MyBrewFolio Sync MSI first. This prevents an old installation or old application data from masking a packaging problem.
2. If you previously tested an older Store test package, open PowerShell in the folder of the old extracted ZIP and run:  PowerShell -ExecutionPolicy Bypass -File .\Uninstall-TestPackage.ps1  Accept the prompt that removes the old temporary certificate. The installer replaces a same-name package automatically, but only this cleanup removes the old certificate.
3. Extract this ZIP file completely to a local folder.
4. Open a normal, non-administrator PowerShell window in the extracted directory.
5. Run:  PowerShell -ExecutionPolicy Bypass -File .\Install-TestPackage.ps1
6. Accept the single Windows administrator prompt. It trusts only the temporary test certificate under Local Computer > Trusted People.
7. Confirm that the script reports a valid signature and a successfully registered package.
8. Confirm that the interface opens both with and without internet access.
9. Test sign-in, OAuth return, tray behavior, window reopening and a manual sync.
10. Close the app and run:  PowerShell -ExecutionPolicy Bypass -File .\Uninstall-TestPackage.ps1
11. Accept the administrator prompt that removes the temporary certificate.

Do not install the certificate manually and do not move it into Trusted Root Certification Authorities.

If package registration fails, send the complete PowerShell output. The script prints the matching AppxDeployment activity log when Windows provides an ActivityId.

If the Microsoft Edge error page appears after successful package registration, do not submit the Partner Center package. Locate the log with:

Get-ChildItem "$env:LOCALAPPDATA\Packages" -Filter startup-diagnostics.log -Recurse -ErrorAction SilentlyContinue

Attach startup-diagnostics.log to the issue report. The file contains app, Windows and WebView2 versions plus startup readiness only. It never contains tokens, local addresses or synchronized data.
