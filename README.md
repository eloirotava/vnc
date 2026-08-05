# rvnc

Servidor VNC em Rust, **um binário só, estático**, que serve um display X para
qualquer navegador. Substitui a pilha `Xvfb + x11vnc + websockify + noVNC` por
um único executável de ~1,3 MB sem nenhuma dependência dinâmica.

```
rvnc xfce4-session
```

Isso sobe um `Xvfb`, roda a sessão nele e serve tudo em `http://0.0.0.0:6080`.
O link impresso no startup já leva direto para a área de trabalho — não existe
tela de conexão intermediária.

```
rvnc: started Xvfb on :1 (1440x900x24)
rvnc: running xfce4-session on :1
rvnc: serving X display :1 (1440x900)
rvnc: open http://localhost:6080/?password=Kf3xpQ7m
rvnc:   or http://localhost:6080/ and enter: Kf3xpQ7m
```

Nada de protocolo inventado: o servidor fala **RFB 3.8** (o protocolo VNC de
verdade) e o cliente é o **noVNC oficial**, embutido no binário em tempo de
compilação.

## Uso

```sh
rvnc xfce4-session                  # sobe o display, roda a sessão, serve
rvnc --display :1                   # serve um X que já está rodando
rvnc -g 1920x1080 -p segredo1 startxfce4
rvnc --listen 127.0.0.1:6080 --view-only --display :0
```

| Opção | O que faz |
| --- | --- |
| `-l, --listen ADDR` | `PORTA` ou `HOST:PORTA` (padrão `0.0.0.0:6080`) |
| `-d, --display NOME` | usa um X existente em vez de subir um `Xvfb` |
| `-g, --geometry WxH` | tamanho do display criado (padrão `1440x900`) |
| `--depth N` | profundidade de cor do display criado (`16`, `24`, `30`) |
| `-p, --password SENHA` | senha VNC (o protocolo usa no máximo 8 caracteres) |
| `--password-file ARQ` | lê a senha da primeira linha do arquivo |
| `--no-password` | serve sem autenticação nenhuma |
| `--view-only` | ignora teclado e mouse dos clientes |
| `--max-fps N` | limite de captura (padrão `30`) |
| `-v, --verbose` | log detalhado |

Sem `--password` nem `--no-password`, o `rvnc` gera uma senha aleatória e a
imprime no startup.

## Build

```sh
cargo build --release                                   # dinâmico, para dev
```

Estático (musl), inclusive cross-compilando de x86_64 para ARM sem docker e
sem `cross` — o projeto não tem nenhuma dependência em C, então o `rust-lld`
resolve o link sozinho:

```sh
rustup target add x86_64-unknown-linux-musl
RUSTFLAGS="-C linker=rust-lld -C target-feature=+crt-static" \
  cargo build --release --target x86_64-unknown-linux-musl
```

Alvos usados nos releases: `x86_64-unknown-linux-musl` (amd64),
`aarch64-unknown-linux-musl` (arm64) e `armv7-unknown-linux-musleabihf`
(armhf).

Em tempo de execução, o `Xvfb` só é necessário quando o `rvnc` precisa criar o
display; com `--display` ele não é usado.

## Releases

O workflow `.github/workflows/release.yml` roda **só quando disparado à mão**
(Actions → Release → Run workflow). Ele executa os testes, compila os três
alvos estáticos, empacota `tar.gz` com `sha256` e publica tudo numa GitHub
Release. Se a tag não for informada, usa a versão do `Cargo.toml`.

## Como funciona

Cinco peças, todas dentro do mesmo processo:

1. **Captura** (`src/x11.rs`) — conecta no X como cliente comum, usa XDAMAGE
   para saber o que mudou e faz `GetImage` só das regiões sujas. Sem damage,
   cai para varredura de tela cheia. XFIXES fornece a imagem do cursor.
2. **Entrada** (`src/x11.rs`) — XTEST injeta mouse e teclado. Keysyms que o
   layout atual não produz são mapeados dinamicamente em keycodes livres, o
   mesmo truque do `x11vnc`, então acentos e símbolos funcionam.
3. **Framebuffer compartilhado** (`src/screen.rs`) — a thread de captura
   escreve num buffer único; cada cliente tem seu próprio bitmap de tiles
   64x64 sujos, que viram retângulos coalescidos na hora de enviar.
4. **RFB** (`src/rfb/`) — handshake 3.8, autenticação VNC (DES), tradução de
   formato de pixel e codificação **ZRLE** (tile sólido, paleta empacotada,
   paleta RLE ou raw, o que for menor, tudo por cima de um stream zlib por
   sessão). Raw serve de fallback.
5. **HTTP/WebSocket** (`src/http.rs`, `src/ws.rs`) — serve o noVNC embutido e
   faz upgrade de `/websockify` para WebSocket, que carrega o RFB puro.

## Segurança

- Sem senha explícita, uma é gerada — o padrão nunca é "aberto".
- Upgrades de WebSocket com `Origin` de outro host são recusados, para que
  uma página qualquer não consiga abrir um socket no seu `rvnc`.
- **Não há TLS.** A autenticação VNC usa DES e é fraca para os padrões de
  hoje. Para expor na internet, coloque atrás de um proxy reverso com HTTPS
  ou use um túnel SSH. Em rede local ou dentro de um container, tudo bem.

## O que ainda não tem

- Área de transferência entre navegador e desktop (`ClientCutText` é lido e
  descartado).
- Redimensionamento dinâmico da tela: a geometria é fixada no startup.
- Codificações Tight/JPEG e CopyRect. Conteúdo fotográfico em tela cheia cai
  em ZRLE raw, que comprime pior que JPEG.
- Displays com paleta (8 bits). Só true colour de 16, 24 e 32 bits.
- Conexão de clientes VNC nativos direto na porta: o `rvnc` só escuta
  HTTP/WebSocket. Para usar um cliente nativo, ponha um `websockify` na
  frente.

## Testes

```sh
cargo test                 # 42 testes de unidade
```

Para o teste ponta a ponta existe um cliente X de apoio que pinta quadrantes
de cores conhecidas, anima um quadrado e marca a tela quando recebe clique ou
tecla:

```sh
cargo run --release --example xdraw     # usa $DISPLAY
rvnc -- ./target/release/examples/xdraw # ou direto pelo rvnc
```

Abrindo o navegador dá para conferir cor exata, atualização incremental e
injeção de entrada de uma vez só.

## Créditos e licença

O cliente embutido é o [noVNC](https://github.com/novnc/noVNC) 1.7.0, sob
MPL-2.0; a cópia vendorizada está em `web/novnc/`, com a licença e o arquivo
de autores originais. O restante do código é MIT.
