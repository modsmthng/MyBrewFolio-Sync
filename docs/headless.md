# MyBrewFolio Sync ohne Desktop

Der Headless-Container verwendet dieselbe Synchronisations-Engine wie die Desktop-App, benötigt aber weder WebKit noch X11. Der Zustand liegt dauerhaft in `/data`; das OAuth-Token wird dort nur verschlüsselt gespeichert.

## Production-Installation

Das offizielle Image enthält die öffentliche OAuth-Client-ID. Nutzer erzeugen daher keine
OAuth-Anwendung und geben keinen Client-Secret an. Der Installer legt ein einmaliges lokales
Verschlüsselungs-Secret, eine persistente Compose-Konfiguration und das Daten-Volume an:

```sh
curl -fsSL https://raw.githubusercontent.com/modsmthng/MyBrewFolio-Sync/main/scripts/install-headless.sh \
  | sh -s -- --host 192.168.1.42
```

Standardmäßig liegt die Konfiguration unter `~/.config/mybrewfolio-sync`. Mit
`MYBREWFOLIO_SYNC_HOME=/srv/mybrewfolio-sync` lässt sich ein anderer Zielpfad wählen; `--no-start`
schreibt nur die Konfiguration. Das erzeugte `state.key` darf weder gelöscht noch weitergegeben
werden, solange der lokale Containerzustand erhalten bleiben soll.

```sh
openssl rand -base64 32 > mybrewfolio-sync-state.key
chmod 600 mybrewfolio-sync-state.key
export MYBREWFOLIO_SYNC_GAGGIMATE_HOST=192.168.1.42
docker compose -f compose.headless.yaml up -d
docker compose -f compose.headless.yaml exec sync mybrewfolio-syncd auth begin
docker compose -f compose.headless.yaml exec sync mybrewfolio-syncd auth wait
```

`auth begin` liefert URL und kurzlebigen Pairing-Code als JSON. Die URL wird im Browser geöffnet; der Container tauscht den resultierenden PKCE-Code selbst gegen Tokens aus. Weder Refresh Token noch der Secret-Schlüssel werden an MyBrewFolio übertragen.

Weitere Beispiele:

```sh
docker compose -f compose.headless.yaml exec sync mybrewfolio-syncd status
docker compose -f compose.headless.yaml exec sync mybrewfolio-syncd sync-once
docker compose -f compose.headless.yaml exec sync mybrewfolio-syncd host set 192.168.1.42
docker compose -f compose.headless.yaml exec sync mybrewfolio-syncd configure reuse-matching
```

Wenn der Daemon läuft, leitet die CLI diese Befehle über den ausschließlich lokalen
`/data/control.sock` an dieselbe Engine weiter. Dadurch erzeugt `docker exec` keine zweite,
gleichzeitige Synchronisation und der Container veröffentlicht keinen Steuerungs-Port.

Wenn `gaggimate.local` im Docker-Bridge-Netz nicht auflösbar ist, muss eine feste LAN-IP bzw. ein lokaler DNS-Name verwendet werden. Auf Linux kann alternativ ein bewusst konfiguriertes Host-Netzwerk genutzt werden. Der Container öffnet keinen TCP-Port und läuft als nicht-root Benutzer.
