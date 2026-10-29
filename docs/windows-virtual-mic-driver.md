# Eigenes Windows-Mikrofon: Treiber statt VB-CABLE?

Antwort auf die Frage, ob HushMic unter Windows ein eigenes Kernel-Gerät mitbringen kann, das
selbst als Mikrofon-Endpunkt erscheint — und was das kostet.

Quellen sind unten mit `[S…]` verlinkt; jede Capability-Aussage hat eine Doku-URL oder eine Messung
am geklonten Sample. Was ich hier nicht beweisen konnte, steht als `[INFERENCE]` mit dem Test, der
es klären würde.

---

## 1. Kurzantwort

Technisch geht es. `ROOT\…`-enumierter ACX-Treiber mit Capture-Circuit, Mikrofon-Endpunkt per INF
statisch deklariert — das ist ein dokumentiertes, von Microsoft gesampletes Muster
[S: acx-ddi-addcircuit], [S: acx-version], [S: measured-acx-inf].

Der Aufwand liegt **nicht** im Code. Er liegt in der Signatur- und Vertriebskette, und die ist seit
2026 härter geworden: Windows verlangt für neue Kernel-Treiber eine WHCP-Signatur, und
Attestation-Signierung ist ausdrücklich als „for testing scenarios" dokumentiert, nicht als
Retail-Weg [S: driver-signing-offerings], [S: windows-driver-policy].

**Empfehlung:** nicht bauen. VB-CABLE bleibt die günstigere Abhängigkeit. Wenn die
Drittanbieter-Abhängigkeit trotzdem weg muss, ist der einzige Entwurf, der den Preis rechtfertigt,
ein Treiber mit **Render-Endpunkt + kernel-seitigem Loopback** auf den Capture-Pin — nicht ein
IOCTL-Datenpfad (Abschnitt 5, Variante D).

---

## 2. Die drei Architekturen

### 2.1 APO — kann keinen Endpunkt erzeugen (belegt, nicht behauptet)

Ein APO ist ein COM-Objekt im User-Mode, ausgeliefert als DLL und im Audio-Engine-Prozess
`audiodg.exe` geladen [S: apo-implementing]. Microsoft schreibt dort wörtlich:

> „An APO can modify only the audio data that is passed to it through its
> `IAudioProcessingObjectRT::APOProcess` routine. **The APO cannot change the settings of the
> underlying logical device, including its KS topology.**"
> — [S: apo-implementing]

Dazu: „Each logical device can be associated with one APO of each type" — ein APO hängt also immer
an einem **schon existierenden** Endpunkt. Und die Registrierung setzt voraus, dass der APO als
PnP-Kind des Audiogeräts installiert wird, „sibling of the audio endpoints"; global registrierbar
ist ein APO nicht [S: apo-implementing].

Endpunkte entstehen an anderer Stelle: der Dienst `AudioEndpointBuilder` überwacht
`KSCATEGORY_AUDIO` und legt für jeden unverbundenen **Bridge-Pin eines KS-Filters** einen Endpunkt
an [S: endpoint-builder-algorithm]. Ein KS-Filter ist ein Treiberobjekt; ohne Treiber kein Pin,
ohne Pin kein Endpunkt. Damit ist die Kette geschlossen: **kein User-Mode-Weg zu einem neuen
Mikrofon-Endpunkt.** Das ist auch der Grund, warum HushMic unter Windows den Default-Eingang nicht
setzen kann — die Aussage im README bleibt korrekt.

Was ein APO trotzdem leisten würde: Rauschunterdrückung **auf dem echten Mikrofon**, ohne
Gerätewechsel, ohne Kabel. Preis: der Denoiser läuft dann im Echtzeitpfad der Audio Engine
(`audiodg.exe`), die die APOs lädt [S: audio-architecture]; dort gilt „must not block, use paged
memory, or call any blocking system routines", „all buffers … nonpageable", „should not introduce
significant latency" [S: apo-implementing]. Unser Stack lädt `onnxruntime.dll` und
`models/*.onnx` (README, Windows-Abschnitt); ob eine ONNX-Runtime in diesem Prozess überhaupt lädt
und die Echtzeitauflagen erfüllt, ist `[INFERENCE]` — klären würde das ein Wegwerf-APO, das ORT
lädt und unter Last läuft. Erschwerend: ein eingebettetes Manifest lässt APIs zu, die in der
„protected environment" verboten sind; der APO läuft dann nur mit dem Testschlüssel
`DisableProtectedAudioDG=1`, ohne ihn „fail to load, even if it is WHQL-signed"
[S: apo-implementing]. Zusätzlich ist das APO-Paket selbst ein Treiberpaket mit vorgeschriebenem
INF-Class (`AudioProcessingObject` ab Win11 21H2, `SoftwareComponent` unter Win10) und wird bei
falscher Klasse zurückgewiesen [S: apo-deploy] — die Signaturpflicht gilt also auch hier.

### 2.2 ACX — der aktuelle Weg

ACX ist eine WDF-Klassenerweiterung für Audio, bewusst auf KMDF statt UMDF gebaut, „to avoid any
latency associated with task-switching multiple times from User to Kernel mode while streaming";
PortCls wird als „the current legacy model" bezeichnet [S: acx-overview]. Version 1.1 ist
„recommended for all new driver development", Mindest-OS Windows 10 2004 mit KMDF ≥ 1.31
[S: acx-version]. `AcxDeviceAddCircuit` „adds an ACX circuit to a WDFDEVICE **to create an audio
endpoint**" [S: acx-ddi-addcircuit] — das ist die Doku-Bestätigung, dass ein ACX-Circuit der
Endpunkt-Erzeuger ist.

Dass ein solcher Treiber als **Mikrofon** erscheint, ist im Sample nicht nur Theorie: die INF des
ACX-Samples deklariert den Capture-Endpunkt statisch über
`AddInterface=%KSCATEGORY_CAPTURE%, … Microphone0` und `Class=MEDIA`
(gemessen: `audio/Acx/Samples/AudioCodec/Driver/AudioCodec.inf`). Der Treiber ist KMDF,
`ACX_VERSION 1.1`, `KMDF 1.31`, `TargetVersion Windows10`
(gemessen: `AudioCodec.vcxproj`).

### 2.3 Klassisch: SysVAD / PortCls

SysVAD ist WDM-PortCls mit WaveRT, KMDF-Wrapper, linkt `portcls.lib` (gemessen:
`audio/sysvad/TabletAudioSample/TabletAudioSample.vcxproj`). Es ist der breiteste Sample —
Tablet, Mic-Arrays, Headset, Bluetooth-HFP, USB-HS, Offload, APO — und genau deshalb der falsche
Ausgangspunkt für ein einzelnes virtuelles Mikrofon: 67 213 Zeilen für 134 Dateien (Abschnitt 3).

Der wichtige Befund: **SysVAD hat keinen User-Mode-Datenpfad.** Der Mikrofon-Capture-Pfad
generiert einen Sinuston — `WriteBytes()` ruft `m_ToneGenerator.GenerateSine(...)` auf
(gemessen: `EndpointsCommon/minwavertstream.cpp:1519`). Das README sagt es selbst: „since it is a
virtual audio driver it does not implement audio mixing and simulates capture and loopback by
generating a tone", und es nennt die Folge: Loopback-Tests scheitern, „because the driver simulates
loopback by returning a sine tone instead of performing real mixing of streams in host and/or
offload pins" [S: sysvad-readme]. Weder `sysvad` noch `Acx` enthalten einen
`EvtIoDeviceControl`-Handler — 0 Treffer, im ganzen `audio/`-Baum nur 3, alle in `SdcaVXu`
(gemessen) —, und nirgends `ZwOpenSection`/`MmMapViewInSystemSpace`. Ein PCM-Transport aus dem
User-Mode in ein virtuelles Mikrofon ist in keinem Microsoft-Audio-Sample implementiert
(Abschnitt 3).

### 2.4 Zur Prämisse „Sideband/VirtualAudioDevice"

Der **Identifikator** `VirtualAudioDevice` kommt im gesamten Repository nicht vor (gemessen:
`grep -rn VirtualAudioDevice .` → 0 Treffer), und es gibt keine Datei dieses Namens. „Virtual audio
device" erscheint nur als Prosatext in den READMEs von drei Samples — SysVAD, Simple Audio Sample
und SdcaVad — und bedeutet dort durchgehend „kein Hardware-Adapter dahinter", nicht „von User-Mode
gespeist" [S: measured]: so heißt es im Simple-Audio-Sample, der Treiber „uses a 'virtual audio
device' instead of an actual hardware-based adapter", während derselbe Treiber seine Capture-Daten
mit `m_ToneGenerator.GenerateSine(...)` erzeugt. Dieselbe Bedeutung hat der Begriff in der
Doku: dort registriert SysAudio die Filtergraphen selbst „as virtual audio devices for playback,
recording, MIDI input/output, and mixing", reserviert dafür `KSCATEGORY_AUDIO_DEVICE` und schreibt
„Adapter drivers should not register themselves in this category" [S: virtual-audio-devices]. Der
Begriff bezeichnet also eine Systemkonstruktion bzw. „keine Hardware", nicht einen
User-Mode-gespeisten Endpunkt. Wer nach einem „VirtualAudioDevice"-Sample sucht, sucht ein Wort,
das die Samples nur im Titel tragen.

„Sideband" ist etwas anderes: `sidebandaudio.h` beschreibt DDIs „for enabling sideband audio for
specific bus types (USB Audio 1.0, A2DP, etc.)" [S: sideband]. Die 24 dort dokumentierten IOCTLs
tragen ausschließlich Deskriptoren, Status, Volume, Mute, Format und Stream-Open/Start/Suspend —
**kein PCM-Payload**. SysVAD nutzt sie in `A2dpHpDevice.cpp`/`BthhfpDevice.cpp` als
Bluetooth-Transport-Metadaten. Wer Sideband als Referenz für einen User-Mode-Feeder liest, liest
die falsche Stelle.

---

## 3. Gemessener Sample-Bestand

Klon: `git clone --depth 1 --filter=blob:none --sparse https://github.com/microsoft/windows-driver-samples /tmp/wds`,
Sparse-Checkout `audio`, HEAD `f4c1ff8` (2026-10-28). Zählung:
`find <dir> -type f \( -name '*.c' -o -name '*.h' -o -name '*.cpp' -o -name '*.hpp' \) | xargs cat | wc -l`.

| Verzeichnis | Dateien | LOC | Treibermodell |
| --- | ---: | ---: | --- |
| `audio/sysvad` (gesamt) | 134 | 67 213 | PortCls/WaveRT (WDM), KMDF-Wrapper |
| `audio/sysvad` ohne `APO`, `KeywordDetectorAdapter` | 105 | 58 728 | dito |
| `audio/sysvad/EndpointsCommon` | 64 | 26 482 | dito |
| `audio/sysvad/TabletAudioSample` | 17 | 6 486 | dito, `Root\sysvad_ComponentizedAudioSample` |
| `audio/sysvad/APO` | 24 | 8 044 | User-Mode-APO |
| `audio/Acx` (gesamt) | 27 | 11 221 | ACX 1.1 / KMDF 1.31 |
| davon `Acx/Samples/Common` | 20 | 9 194 | geteilt: Render-, Capture-, HDMI-Circuit, WAV, Tone |
| davon `Acx/Samples/AudioCodec` | 3 | 622 | nur `Driver.cpp`, `Device.cpp`, `DriverSettings.h` + INF |
| davon `Acx/Samples/Inc` | 2 | 1 030 | `Private.h` (Circuit-/Pin-IDs), `AcxTrace.h` |
| `audio/simpleaudiosample` | 32 | 15 351 | PortCls/WaveRT, `ROOT\SimpleAudioSample` |

Virtuelle Audiogeräte im Sinne der Frage: `sysvad` (Speaker/Mic-Array/Headset/BT/USB, alle ohne
Hardware-Unterbau), `Acx/Samples` (Speaker + Microphone + HDMI), `simpleaudiosample`
(Speaker + Mic-Array). Alle drei exponieren ein Mikrofon als Endpunkt. Keines nimmt seine Samples
von außen an. Microsoft beschreibt die Sammlung selbst als „working implementations with source code
that can serve as a starting point for writing a custom driver for a proprietary audio device"
[S: sample-drivers] — als Startsignal für eigene Treiber, nicht als fertiges virtuelles Mikrofon.

Lizenz: ein `LICENSE` im Repo-Wurzelverzeichnis, **Microsoft Public License (MS-PL)**; nur
`audio/SoundWire` hat eine eigene. MS-PL erlaubt Derivate und die Auslieferung in Objektform unter
einer kompatiblen Lizenz, verlangt aber den Erhalt der Copyright-/Attributionshinweise und bei
Quellcode-Weitergabe die Beilage der vollständigen Lizenz (Abschnitt 3(C)/(D), gemessen). Forken
ist also lizenzrechtlich unproblematisch.

---

## 4. Signieren und Verteilen — die eigentliche Hürde

### 4.1 Zertifikat und Konto

- Ein EV-Code-Signing-Zertifikat ist Voraussetzung für das Hardware-Dev-Center-Konto: „You must
  have an extended validation (EV) code signing certificate"; falls nicht vorhanden, „you must
  purchase an EV certificate" [S: hw-register]. Interessante Nuance: „You only need the certificate
  to register for the Hardware Developer Program. You don't need to sign your driver with it." —
  signiert wird von Microsoft, das Zertifikat ist der Identitätsnachweis fürs Konto [S: hw-register].
  Für Attestation-Einreichungen wird es ebenfalls verlangt („requires the use of an extended
  validation certificate to submit the driver to Partner Center") [S: driver-signing-offerings].
  Wichtig für die Kosten: Pflicht ist das EV-Zertifikat nur **am Konto**. Die einzelne Einreichung
  darf mit einem gewöhnlichen Authenticode-Zertifikat signiert werden — „Although Microsoft strongly
  recommends that you sign individual submissions with an EV certificate, you can sign submissions
  with an Authenticode signing certificate" [S: code-signing-reqs].
- Konto nötig, weil: „Starting with Windows 10, version 1607, Windows will not load any new
  kernel-mode drivers which are not signed by the Dev Portal" [S: kmcs-policy]. Ausnahmen
  (Upgrade von einer älteren Windows-Version, Secure Boot aus, Zertifikat vor dem 29.07.2015)
  treffen einen Neuvertrieb nicht.
- Registrierung verlangt zusätzlich einen Microsoft-Entra-ID-**Global-Administrator** und einen
  rechtsverbindlich zeichnungsberechtigten Kontakt; ohne beide ist die Registrierung nicht
  abschließbar („You can't complete registration without both prerequisites") [S: hw-register].
- **Preise:** auf den von mir gelesenen Microsoft-Seiten steht kein Preis — weder für das
  Zertifikat noch für das Konto. Ich schreibe daher **nicht veröffentlicht**, nicht „kostenlos"
  und nicht „einige hundert Euro".

### 4.2 Attestation vs. WHQL/HLK

| | Attestation | WHQL/HLK |
| --- | --- | --- |
| HLK-Tests nötig | nein, „doesn't require HLK testing" | ja, HLK-Lauf je Zielversion |
| Retail-Verteilung | „can't be published to Windows Update for retail audiences" | ja |
| Zertifiziert | „it's not Windows Certified … no assurances about compatibility or functionality" | ja |
| OS | „only works on Windows 10 Desktop and later" | Vista+ inkl. nachweislich bestandener HLK |
| Windows Server | „doesn't accept attested device and filter driver signing submissions" (2016+) | nur bestandene HLK |

Quellen: [S: driver-signing-offerings], [S: attestation], [S: kmcs-policy].

Der entscheidende Satz für die Distribution steht in der Attestation-Doku selbst: die Überschrift
lautet „Attestation signed drivers **for testing scenarios**" [S: driver-signing-offerings]. Ein so
signierter Treiber darf also nicht der Weg sein, auf dem ein Endnutzer HushMic installiert.

Neben Attestation und HLK gibt es noch **Preproduction Signing**: laden mit aktivem Secure Boot,
aber nur auf Geräten, die ausdrücklich auf diese Signatur provisioniert wurden, und „Drivers don't
load on retail systems unless those systems are explicitly provisioned to trust the preproduction
signature" [S: driver-signing-offerings]. Das ist der realistische Weg für unseren eigenen
Test-Signing-Lauf auf einem Prüfrechner mit eingeschaltetem Secure Boot — ohne das BIOS zu
verbiegen.

### 4.3 Die neue Windows Driver Policy

Microsofts Support-Seite „The Windows Driver Policy" sagt:

> „Windows requires all new drivers to be submitted and signed through the Windows Hardware
> Compatibility Program (WHCP) process. Windows previously trusted drivers signed by the
> now-expired cross-signed program. However, with the April 2026 security update, these drivers are
> no longer trusted by default."
> — [S: windows-driver-policy]

Im Enforcement-Modus lädt der Kernel nur noch (1) WHCP-signierte Treiber und (2) Treiber auf der
Allow-List der Policy. Das WHCP ist Microsofts Kompatibilitätsprogramm, in dem genau diese
Zertifizierung stattfindet [S: whcp]. Der Modus beginnt in einer **Evaluationsphase**, in der
blockierbare Treiber „are audited but still allowed to load", und schaltet automatisch scharf,
sobald 250 Stunden aktiver Nutzung und 3 Reboots ohne Policy-Verstoß zusammenkommen; ein Verstoß
setzt beide Zähler auf Null zurück, und ein einzelner Treiber lässt sich nicht freischalten
(„There is currently no way to bypass the policy for individual drivers"). Das Feature betrifft
**nur Kernel-Treiber**, keine User-Mode-Anwendungen — der Rust-Prozess bleibt außen vor, der Treiber
nicht [S: windows-driver-policy].

Damit ist die Attestation-Frage entschieden, und zwar ohne Inferenz: die Seite sagt es selbst.
Zulässig sind „Drivers properly signed through the Microsoft WHCP certification process" und
Allow-List-Treiber aus dem *Cross-Signing*-Programm; „Drivers that are not Microsoft WHCP signed or
appear on the Windows Driver policy will be blocked on in scope, enabled systems." Ein
attestation-signierter Treiber ist „not Windows Certified" [S: driver-signing-offerings] und damit
weder das eine noch das andere. Dass er blockiert wird, ist die dokumentierte Folge, keine
Schlussfolgerung [S: windows-driver-policy].

Zwei Erleichterungen, die im Text stehen und für die Entscheidung zählen: erstens ist die
Enforcement-Aktivierung kein Schicksal, sondern abschaltbar — ab dem Juli-2026-Update genügt
`CiTool.exe --remove-policy "{8F9CB695-5D48-48D6-A329-7202B44607E3}"` mit Admin-Rechten und ein
Neustart (ohne Secure-Boot-Änderung), davor ist zusätzlich ein Ausflug ins UEFI nötig. Zweitens
nennt die Seite nicht, welche Editionen „in scope" sind — welche Windows-Editionen die Policy also
tatsächlich bekommen, bleibt offen und wäre Teil eines Tests (Event ID 3077, Policy-ID
`{8F9CB695-5D48-48D6-A329-7202B44607E3}`, Log `Microsoft-Windows-CodeIntegrity/Operational`).
Praktisch heißt das trotzdem: **Retail braucht HLK.** Die HLK-Voraussetzungen sind ebenfalls
dokumentiert — zwei Testrechner, Loopback-Kabel, für den Fidelity-Test ein Audio Precision System
Two [S: hlk-audio-prereqs].

### 4.4 Installation, Rechte, Secure Boot

- Treiberinstallation braucht Admin: DevCon „the user must be a member of the Administrators group
  … and run DevCon from an elevated command prompt" [S: devcon]; `pnputil` ebenso („Run as
  administrator") [S: pnputil].
- Testsignierte Treiber laden standardmäßig gar nicht; `bcdedit -set TESTSIGNING ON` braucht
  Admin **und einen Reboot**, und Secure Boot blockiert den Schalter („The value is protected by
  Secure Boot policy and cannot be modified or deleted" → Secure Boot im BIOS abschalten,
  BitLocker kann zusätzlich stören) [S: testsigning]. Ab Win10 1507 verlangt HVCI zusätzlich
  mindestens eine test-signierte Binärdatei.
- Nicht vertrauenskette → Gerät startet nicht: Fehlercode 52, „Windows cannot verify the digital
  signature for the drivers required for this device" [S: code52]. Der einzige Endnutzer-Fix ist
  ein korrekt signierter Treiber.
- Windows 10 in S Mode akzeptiert nur Windows-/WHQL-/ELAM-/Store-signierte Pakete
  [S: s-mode] — Attestation fällt dort ohnehin aus.

### 4.5 Versionsbindung

ACX 1.1 setzt Windows 10 2004 / KMDF 1.31 voraus [S: acx-version]. Windows 10 Home/Pro ist laut
Lifecycle-Seite aus dem Support: die Tabelle nennt „10/15/2025 6:59:59 AM" (UTC), der Fließtext
„October 14, 2025" — beides derselbe Zeitpunkt, 22H2 war die letzte Version
[S: win10-eol]. Praktisch ist die Untergrenze damit Windows 11; das ist unkritisch, weil ACX dort
vorhanden ist, aber es gehört in die Entscheidung: der Treiber bringt keine Alt-Windows-Unterstützung,
die die Kabel-Lösung hat (VB-CABLE nennt für Paket 45, 1,29 MB, Stand Oktober 2024:
„XP to WIN11 32/64 bits/Arm64").

---

## 5. Der Datenpfad: wie kommen die Samples in den Treiber?

Fünf Varianten, davon vier mit Treiber. Wichtig für alle: ein `ROOT\…`-Gerät wird bei der Installation
angelegt und existiert unabhängig von unserem Prozess — der Endpunkt verschwindet **nicht**, wenn
HushMic abstürzt. Damit ist die Frage nicht „Gerät weg?", sondern „was liefert der Treiber, wenn
niemand füttert?".

| # | Variante | In Samples belegt? | Wenn HushMic nicht läuft | Kosten/Risiko |
| --- | --- | --- | --- | --- |
| A | IOCTL-Streaming: App schreibt PCM via `DeviceIoControl`, Treiber hält Ringpuffer | **nur als Gerüst**: in `sysvad`/`Acx` **0** `EvtIoDeviceControl`-Handler; der einzige IOCTL-Treiber unter `audio/` ist `SoundWire/Samples/SdcaVad/SdcaVXu` (3 Treffer), und sein `IOCTL_SDCAVXU_INTERFACE_TEST` überträgt ein `ULONG`, kein PCM (gemessen) | Ring läuft leer → muss **Stille** ausgeben; ein naiver „letzten Block wiederholen"-Puffer erzeugt einen hängenden Ton. Endpunkt bleibt sichtbar. | Eigener Transport, Underrun-Policy, ACL gegen fremde Einspeisung; Latenz = Puffertiefe + Kopie |
| B | Shared Section: App mappt Ringpuffer, Treiber pollt/Event | **nein** — kein `ZwOpenSection`/`MmMapViewInSystemSpace` unter `audio/` (gemessen, 0 Treffer) | wie A, zusätzlich Tearing-Risiko und CPU-Last durch Polling | ohne Vorbild komplett selbst zu bauen |
| C | ACX-Stream-Pins an User-Mode-Feeder brücken | **nein** — Bridge-Pins verbinden Circuits im Kernel. Der einzige dokumentierte User→Kernel-Kanal ist `KSPROPERTY_AUDIOMODULE_COMMAND` (Win10 1703), und der überträgt Kommandos/Parameter, kein kontinuierliches PCM [S: audiomodule] | n/a — landet am Ende wieder bei A oder B | keine Abkürzung |
| D | Eigener **Render**-Endpunkt, dessen Loopback-Pin kernel-seitig in den Capture-Pin kopiert | **Topologie ja, Datenweg nein**: der Loopback-Pin existiert und wird im Graphen verdrahtet (`Acx/Samples/Common/CircuitHelper.cpp:177`; `RenderLoopbackPin` in `sysvad/EndpointsCommon/speakerhpwavtable.h:465`). Aber `CodecR_EvtStreamGetLoopbackPacket` u. a. sind **nur deklariert, nie definiert** (0 Verwendungen außerhalb `Private.h`), und SysVAD liefert auf Loopback einen Sinuston statt zu mischen: „the driver simulates loopback by returning a sine tone instead of performing real mixing of streams" [S: sysvad-readme], [S: measured] | Render-Stream endet → Loopback hat keine Daten → **Stille** | Bester Entwurf: Rust rendert unverändert per WASAPI in ein Gerät, das jetzt „HushMic Input" heißt. Kein neuer Transport, eine Kernel-Kopie, kein User-Mode-Hop — aber die Kopie selbst ist neu zu schreiben |
| E | **Gar kein Treiber**: WASAPI-Loopback-Capture statt virtuellem Kabel | ja, aber als Client-Funktion, nicht als Gerät: „a client of WASAPI can capture the audio stream that is being played by a rendering endpoint device"; nur shared mode, „Exclusive-mode streams cannot operate in loopback mode" [S: loopback-recording] | n/a | **Löst die Aufgabe nicht.** Loopback invertiert die Richtung: *unser* Prozess dürfte mitschneiden, was ein Render-Endpunkt abspielt — gesucht ist aber das Gegenteil, ein Endpunkt, den OBS/Zoom als Mikrofon öffnen. Kein Gerät, keine Installation, aber auch kein Mikrofon für Dritte |

Variante D ist die einzige, die den bestehenden Rust-Pfad nicht anfasst: `pump.rs` schreibt weiter
per WASAPI in ein Render-Gerät, und HushMic akzeptiert den Zielnamen bereits frei — `HUSHMIC_SINK`
überschreibt die Suchliste (`main.rs:2623`; `win/chain.rs:227` löst den Namen in `resolve_render`
auf). Ein eigener Render-Endpunkt ist damit **ohne Codeänderung** adressierbar; nur die
Default-Suchliste in `win/chain.rs:38` bzw. `win/doctor.rs:381` würde den neuen Namen aufnehmen.
Alles andere (A/B/C) bedeutet einen zweiten, selbst erfundenen Datenkanal neben WASAPI.

Die Latenz von D (eine Kopie im Kernel statt zwei Endpunkte hin und zurück) ist
`[INFERENCE]` niedriger als heute; ohne Windows-Host nicht messbar. Die 80 ms aus dem README sind
Engine-Latenz (2400 + 1440 Samples) und bleiben davon unberührt.

---

## 6. Vergleich mit dem Status quo

| | Heute (VB-CABLE) | Eigener ACX-Treiber |
| --- | --- | --- |
| Schritte für den Nutzer | Kabel installieren: „Run Setup Program in administrator mode. Reboot after installation", dann `CABLE Output` als Mikrofon wählen [S: vbcable] | Treiber installieren (Admin), Mikrofon wählen; Reboot `[INFERENCE]` nicht nötig |
| Admin nötig | ja | ja |
| Signatur nötig | nicht von uns — der Kabel-Hersteller trägt sie | ja: EV-Zertifikat + Konto + HLK/WHCP (Retail), sonst Attestation mit Policy-Risiko |
| Kosten für uns | Lizenz „Donationware", Preis selbst wählbar („you can adjust the License Price to your means or usage") [S: vbcable]; Abhängigkeit von Dritten | Zertifikat (**Preis nicht veröffentlicht**), HLK-Hardware, Kernel-C-Entwicklung dauerhaft |
| Update-Kopplung | lose: Kabel ist ein separates Produkt, Versionsdrift unkritisch | eng: Treiberversion ↔ App-ABI (Puffergrößen, Formate, IOCTL-Struktur), jeder Treiberwechsel ist ein Setup-/Reboot-Thema |
| Wenn die App stirbt | Kabel bleibt, liefert Stille | Endpunkt bleibt, liefert Stille (bei D) bzw. muss Stille erzwingen (bei A/B) |
| Wenn der Treiber kaputt ist | fremdes Produkt, fremder Support | Bugcheck beim Nutzer, unser Support, Code-52-Installationsfehler |
| Sprache/Build | — | C/C++ mit WDK + Visual Studio, nicht Rust; eigener Build- und Release-Zweig |
| Windows-Versionen | XP–Win11 laut Hersteller | ACX: Win10 2004+, praktisch Win11 |

Status quo, gemessen an unserem Code: `doctor.rs` sucht das Render-Gerät über die Namen
`CABLE Input`, `VoiceMeeter Input`, `VB-Audio`; der Nutzer muss das Gegenstück `CABLE Output` als
Mikrofon auswählen, weil Windows keinen unterstützten Weg bietet, den Standardeingang umzubiegen
(README, Windows-Abschnitt).

---

## 7. Was hier nicht beweisbar ist

Kein Windows-Host verfügbar. Damit sind **nicht** belegt und bleiben offen:

- dass der Treiber lädt und einen Endpunkt erzeugt (Devcon-Install, Geräte-Manager, `KSCATEGORY_CAPTURE`);
- jede Latenzzahl für Variante D;
- dass ein attestation-signierter Treiber im Enforcement-Modus tatsächlich blockiert wird — die
  Policy-Seite sagt es für „in scope, enabled systems", nennt aber die Editionen nicht, und der
  Enforcement-Zustand ist zustandsabhängig (Diagnose: `citool -lp` bzw. Event ID 3077);
- HLK-Lauf und Zertifizierungsergebnis;
- ob eine ONNX-Runtime in `audiodg` für den APO-Weg lädt und echtzeitfähig bleibt.

Ein Test-Signing-Lauf bräuchte: Windows 10 2004+ oder Windows 11 x64 (VM genügt für den
Load-Test, nicht für Latenz), WDK + Visual Studio, Admin, Secure Boot aus im UEFI (sonst lässt
sich `TESTSIGNING` nicht setzen) bzw. ein selbst erzeugtes Testzertifikat bei aktivem HVCI,
`bcdedit -set TESTSIGNING ON` + Reboot, `devcon install` der gebauten INF, danach `ksstudio` oder
ein WASAPI-Client, der den neuen Endpunkt öffnet [S: testsigning], [S: devcon].
Für die Policy-Frage zusätzlich ein Gerät mit Juli-2026-Update oder später im
Enforcement-Modus (`citool -lp` zeigt `IsEnforced`); die Diagnose auf Blockade läuft über
Event ID 3077 im Log `Microsoft-Windows-CodeIntegrity/Operational` [S: windows-driver-policy].

---

## 8. Empfehlung

**Nicht bauen.** Der Treiber würde die Kabel-Abhängigkeit technisch entfernen, aber er tauscht
eine sichtbare, günstige, vom Nutzer einmalig installierte Fremdkomponente gegen eine dauerhaft
selbst zu wartende, signierte Kernel-Komponente — mit EV-Zertifikat, Partner-Center-Konto,
HLK-Zertifizierung für den Retail-Weg und einem C-Build-Zweig, den dieses Projekt sonst nirgends
hat. Der Nutzen für den Anwender ist eine Installationsseite weniger; der Preis ist eine
Produktkategorie mehr.

Falls die Abhängigkeit aus Produktgründen fallen muss, in dieser Reihenfolge:

1. **Prüfen, ob das Kabel mitgeliefert/lizenziert werden darf.** Dann bleibt der Code unverändert,
   nur der Installer wächst. Die Redistributionsbedingungen von VB-CABLE habe ich nicht geprüft —
   offener Punkt, würde eine Anfrage beim Hersteller klären.
2. **Sonst Variante D** als ACX-Treiber: Render-Endpunkt plus kernel-seitiger Loopback auf den
   Capture-Pin, ein Mikrofon-Endpunkt, statisch in der INF deklariert. Kleinster Eingriff in Rust
   (Gerätename), kein neuer Transport, wohldefiniertes Verhalten bei App-Absturz.
3. **Nicht** Variante A/B als Erstentwurf: eigener IOCTL-/Shared-Memory-Transport ohne jedes
   Sample-Vorbild, mit Underrun- und Injectionsfläche.

Unvermeidbare Kosten, egal welche Variante: EV-Zertifikat (Preis nicht veröffentlicht) und
Hardware-Programm-Konto; für Retail HLK-Tests und -Hardware; Admin-Recht beim Installieren;
dauerhafte Pflege von Kernel-C; Kopplung zwischen Treiber- und App-Version; Windows-11-Untergrenze.

Was die Empfehlung kippen würde: ein Beleg, dass ein attestation-signierter Treiber auf einem
Gerät im Enforcement-Modus lädt — dann entfiele die HLK-Strecke und damit der größte Posten. Nach
der Policy-Seite ist das nicht zu erwarten („Drivers that are not Microsoft WHCP signed or appear
on the Windows Driver policy will be blocked"), aber genau dieser Satz ist der einzige, der die
Kostenrechnung trägt, und er ist mit einem Prüfrechner in einer Stunde widerlegbar [S:
windows-driver-policy]. Der zweite Kippfaktor liegt außerhalb der Technik: ob VB-CABLE
mitgeliefert werden darf (Punkt 1) — dann ist die ganze Frage gegenstandslos.

---

## Quellen

Alle hier zitierten Seiten habe ich in dieser Sitzung abgerufen; die Samples stammen aus dem
Klon unter `/tmp/wds` (HEAD `f4c1ff8`, 2026-10-28).

| Kürzel | URL |
| --- | --- |
| S: apo-implementing | https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/implementing-audio-processing-objects |
| S: apo-deploy | https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/deploying-audio-processing-objects |
| S: endpoint-builder-algorithm | https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/audio-endpoint-builder-algorithm |
| S: acx-overview | https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/acx-audio-class-extensions-overview |
| S: acx-version | https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/acx-version-overview |
| S: acx-ddi-addcircuit | https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/acxdevice/nf-acxdevice-acxdeviceaddcircuit |
| S: sideband | https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/sidebandaudio/ |
| S: audiomodule | https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/implementing-audio-module-communication |
| S: sysvad-readme | https://github.com/microsoft/windows-driver-samples/blob/main/audio/sysvad/README.md |
| S: measured | Messung im Klon `/tmp/wds` (Zeilen-/Dateizahlen in Abschnitt 3, `grep`-Befunde in 2.3/2.4, Codesuche in 2.1) |
| S: measured-acx-inf | `/tmp/wds/audio/Acx/Samples/AudioCodec/Driver/AudioCodec.inf` (`ROOT\AudioCodec`, `Class=MEDIA`, `AddInterface=%KSCATEGORY_CAPTURE%`) |
| S: driver-signing-offerings | https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/driver-signing-offerings |
| S: attestation | https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-attestation |
| S: code-signing-reqs | https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-reqs |
| S: hw-register | https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/hardware-program-register |
| S: kmcs-policy | https://learn.microsoft.com/en-us/windows-hardware/drivers/install/kernel-mode-code-signing-policy--windows-vista-and-later- |
| S: windows-driver-policy | https://support.microsoft.com/en-US/Windows/Hardware/Drivers/the-windows-driver-policy |
| S: testsigning | https://learn.microsoft.com/en-us/windows-hardware/drivers/install/the-testsigning-boot-configuration-option |
| S: code52 | https://learn.microsoft.com/en-us/windows-hardware/drivers/install/cm-prob-unsigned-driver |
| S: devcon | https://learn.microsoft.com/en-us/windows-hardware/drivers/install/using-the-devcon-tool-to-install-a-driver-package |
| S: pnputil | https://learn.microsoft.com/en-us/windows-hardware/drivers/devtest/pnputil |
| S: s-mode | https://learn.microsoft.com/en-us/windows-hardware/drivers/install/windows10sdriverrequirements |
| S: hlk-audio-prereqs | https://learn.microsoft.com/en-us/windows-hardware/test/hlk/testref/audio-device-testing-prerequisites |
| S: win10-eol | https://learn.microsoft.com/en-us/lifecycle/products/windows-10-home-and-pro |
| S: vbcable | https://vb-audio.com/Cable/ |
| S: sample-drivers | https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/sample-audio-drivers |
| S: audio-architecture | https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/windows-audio-architecture |
| S: loopback-recording | https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording |
| S: virtual-audio-devices | https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/virtual-audio-devices |
| S: whcp | https://learn.microsoft.com/en-us/windows-hardware/design/compatibility/ |
