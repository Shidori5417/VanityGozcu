# Kod asistanı için prompt

Aşağıdaki metni, bu klasördeki diğer md dosyalarıyla birlikte bir AI kod asistanına ver.

---

Rust ile **VanityGözcü** adında bir Discord vanity URL izleme aracı yazmanı istiyorum. Ayrıntılı tasarım bu repodaki `README.md`, `MIMARI.md`, `YAPILANDIRMA.md`, `GUVENLIK.md` ve `YOL_HARITASI.md` dosyalarında. Önce hepsini oku, sonra `YOL_HARITASI.md`'deki aşamalara sırayla uy; bir aşama bitip testleri geçmeden diğerine başlama.

**Özet gereksinimler:**
- Tek binary, `tokio` async, `reqwest` (rustls), `ratatui` + `crossterm` TUI.
- ≤20 vanity kodunu `GET /api/v10/invites/{code}` ile kontrol et; 404 + `code 10006` = boş. Yanlış pozitif için ardışık onay (`confirm_checks`).
- Boşalınca Discord webhook ve Telegram bildirimi.
- Opsiyonel, varsayılan kapalı otomatik alma: `PATCH /api/v10/guilds/{id}/vanity-url`, kullanıcı token'ı env'den. MFA/captcha istenirse **atlatmaya çalışma**, dur ve bildir. Proxy/çoklu token rotasyonu ekleme.
- Discord rate limit başlıklarına tam uy; 429'da global bekleme; hatalarda exponential backoff.
- Gizli bilgiler sadece env'den, `secrecy` ile; loglarda asla görünmesin.
- SQLite ile durum kalıcılığı; yeniden başlatmada tekrar bildirim yok.
- Komutlar: `run` (TUI), `daemon` (headless/systemd), `check <kod>`, `test-notify`.

**Kod kalitesi:**
- Modül yapısı `MIMARI.md`'deki gibi olsun.
- Kütüphane hataları `thiserror`, `main` tarafında `anyhow`.
- `unwrap()`/`expect()` yalnızca testlerde.
- Checker, state ve claimer için `wiremock` ile birim testleri.
- `cargo clippy -- -D warnings` temiz geçsin.
- Release profili: `lto = true`, `codegen-units = 1`, `strip = true`, `panic = "abort"` (TUI terminal geri yüklemesini panic hook ile yap).
- Performans öncelikli: tek `reqwest::Client`, gereksiz clone/allocation yok, boşta düşük RAM.

Her aşamanın sonunda: ne yaptığını, nasıl test ettiğini ve sonraki aşamada ne yapacağını kısaca yaz.
