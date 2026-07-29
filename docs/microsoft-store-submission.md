# Microsoft Store submission

## Store description opening

The first two lines of the Partner Center description must read:

> Requires Microsoft Visual C++ Runtime. MyBrewFolio Sync copies shots, profiles, and notes from a GaggiMate espresso machine on the same local network to the user's private MyBrewFolio library.

## Certification notes

Use the following notes for the 0.2.8 submission:

> The Microsoft Visual C++ framework dependency is now declared as Microsoft.VCLibs.140.00.UWPDesktop in the package manifest. The application interface is embedded in the MSIX and does not load a remote website at startup. The package was tested from a clean install on Windows 11, online and offline. Internet access is required for account sign-in and cloud synchronization. Synchronization requires a GaggiMate espresso machine on the same local network. The included full-trust desktop process is required for direct local-machine communication, background synchronization, tray integration, autostart and secure OS credential storage.

## Certification test instructions

Copy this short instruction into Partner Center with the certification notes:

> Launch MyBrewFolio Sync normally from the Windows Start menu. No command-line arguments or prior configuration are required. The embedded setup interface must open without a GaggiMate and while offline. Internet access is required for account sign-in and cloud synchronization. A complete synchronization requires a GaggiMate espresso machine on the same local network.

## Pre-submission test

1. Download `MyBrewFolio-Sync-Store-Test.zip` from the private `store-v0.2.8` draft.
2. Use a Windows 11 account without a previous MyBrewFolio Sync MSI installation.
3. Follow the included `README.txt` and run `Install-TestPackage.ps1`.
4. Start the application online and offline. Its embedded interface must render in both cases.
5. Test OAuth return, tray actions, window reopening and a manual synchronization.
6. If an Edge WebView2 error page appears, stop the submission and collect `startup-diagnostics.log`.
7. Run `Uninstall-TestPackage.ps1` after the test.

The unsigned `MyBrewFolio-Sync-Store.msix` is the only file submitted to Partner Center. The test ZIP and certificate are never submitted or published.
