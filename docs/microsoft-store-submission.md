# Microsoft Store submission

`MyBrewFolio-Sync-Store.msix` is the unsigned package intended for Microsoft
Partner Center. Upload this file to a new submission for the existing
MyBrewFolio Sync product. Do not upload `MyBrewFolio-Sync-Store-Test.zip` to
Partner Center; it is a local Windows installation test bundle only.

The MSIX declares the opt-in `MyBrewFolioSyncStartup` startup task. When a
customer enables **Start Sync with this computer**, Windows asks for the final
permission to run it at sign-in. The task launches Sync in the tray; a normal
manual app launch continues to open its window.

Before submitting, install the test ZIP on a Windows machine with
`Install-MyBrewFolioStoreTest.ps1`. Confirm that the app opens normally when
launched manually, then enable startup, sign out and back in, and confirm that
Sync is running from the notification-area icon. Run
`Remove-MyBrewFolioStoreTest.ps1` afterwards to remove the test package and
temporary certificate.
