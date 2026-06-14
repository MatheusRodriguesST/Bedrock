# Bedrock — guia do projeto (para o Claude e para mim)

## Ideia principal
Bedrock é um **storage engine / mini banco de dados durável**, escrito em **Rust**, feito como
projeto de portfólio. O objetivo é **aprender sistemas implementando**: storage em disco,
durabilidade, recuperação pós-crash, índices e concorrência. O foco é fazer um projeto
extremamente avançado para chamar atenção de recrutadores, mostrando experiência de backend
de nível **pleno a sênior**.

O que é: um motor de armazenamento que guarda dados em disco num **formato que eu controlo**,
com uma linguagem de query mínima ou API HTTP, lidando com múltiplos clientes concorrentes de
forma segura (locking ou versionamento), e com **durabilidade real** — os dados sobrevivem a
restart e a crash.

Por que impressiona: é o projeto que mais separa "sabe framework" de "entende sistemas".
Toca em tudo que recrutador de backend valoriza: storage em disco, concorrência,
durabilidade, recuperação.

Conceitos que o projeto prova: write-ahead log (WAL), flush/fsync, recuperação pós-crash,
índices (LSM-tree), controle de concorrência. **Documentar as garantias oferecidas**
(atomicidade, isolamento, durabilidade).

Nível pleno+: LSM-tree (como o que roda por baixo do RocksDB/Cassandra) com compaction,
ou MVCC para isolamento de transações.

## Workspace Cargo
- `crates/storage-engine` — o motor (binário, onde mora a lógica)
- `crates/server` — futura API HTTP / query (ainda vazio)

## Decisões de design já firmadas
- **NÃO usar JSON** como formato de armazenamento. O ponto é controlar o formato em disco.
- Formato v1 do log: **textual, append-only** — `SET\tchave\tvalor\n` e `DEL\tchave\n`.
  Depois migra para **binário com tamanho-prefixado** (grava "valor tem N bytes", lê N bytes
  crus) — resolve a limitação do v1 com tab/newline no valor.
- Modelo de base: **log append-only + índice em memória** (estilo Bitcask, cap. 3 do DDIA).

---

## Como o Claude deve trabalhar comigo (IMPORTANTE)
- Eu sou **aluno**; o Claude é **instrutor**. Nível de Rust: **intermediário**.
- **NÃO escrever o código de implementação por mim.** O papel do Claude é: explicar
  conceitos, especificar as funções (assinatura + o que cada uma faz + decisões/edge cases),
  apontar armadilhas, e **me deixar implementar**. Pode escrever scaffolding/testes
  não-centrais e revisar meu código.
- Trechos de ≤3 linhas para ilustrar sintaxe de Rust são ok. Implementação completa de uma
  função central, não.
- Quando eu travar, me dê pistas e perguntas que me empurrem, não a resposta pronta. Assumo
  uns 30-45 min de luta antes de pedir socorro — se o Claude entregar fácil demais, perco o
  aprendizado que é o ponto do projeto.
- Sempre ser **honesto sobre as garantias reais** do código atual. Toda afirmação de
  durabilidade DEVE dizer se é a **restart limpo** ou a **crash** (são coisas diferentes:
  restart limpo só exige replay; crash exige fsync).

---

## Onde paramos (atualizar a cada sessão)
- ✅ FEITO: `open`/`set`/`get`/`delete` no **formato binário tamanho-prefixado**
  (`[checksum u32][op u8][key_len u32][val_len u32][key][val]`, little-endian), com **fsync
  por escrita** (`sync_all`) + **CRC32 por registro** (crate `crc32fast`) + replay
  **tolerante a torn write** (para na cauda rasgada via CRC/leitura curta).
- ✅ FEITO: **teste de crash com SIGKILL** (`tests/crash_recovery.rs` + `crash_writer`) — #2.
- ✅ FEITO (degrau 6): **índice de offsets** — índice guarda `ValueLoc { offset, len }`,
  valor fica no disco (`get` faz seek+read). Dataset pode passar da RAM. Bitcask completo.
- ✅ FEITO: **benchmarks** (criterion, `cargo bench`) com **baseline SQLite** (durabilidade
  igualada) e **README profissional em inglês** (#1, #3). Código todo em **inglês**.
- ✅ FEITO (degrau 7): **segmentos + compaction**. Manifesto (`manifest`) = fonte da verdade
  dos segmentos vivos; compaction síncrona por tamanho mescla imutáveis, swap **atômico** do
  manifesto (crash-safe), órfãos limpos no `open`. + truncate-on-recovery. `debug_status`
  (introspecção read-only) pro observer em `playground/observer`.
- ⏭️ PRÓXIMO (degrau 8): concorrência — `RwLock` para leituras concorrentes, depois **MVCC**
  (sequence numbers + snapshot isolation). Ver [[07-concorrencia-mvcc]] nas notas.
- 🧭 GARANTIA ATUAL: durabilidade a **crash** (assumindo que o disco honra o fsync).
  Números: `set` ~2,6 ms (vs SQLite ~2,8 ms), `get` ~0,77 µs (vs SQLite ~2,15 µs).

## Roadmap em degraus (cada um resolve a limitação do anterior)
1. **[FEITO]** `struct Db` + `open` (abre/cria o log, índice em memória)
2. **[FEITO]** `set` / `get` — escrever no log e ler do índice
3. **[FEITO]** replay do log no `open` (reconstruir o índice no boot) → durabilidade a restart
4. **[FEITO]** `delete` via tombstone (+ replay entende `DEL`); crate virou lib (`Db`) + bin
5. **[FEITO]** `fsync` (`sync_all`) + checksum por registro → durabilidade a **crash** (WAL de verdade);
   replay tolera registro rasgado (torn write)
6. **[FEITO]** Índice de offsets (guarda chave→posição, não o valor) → estilo Bitcask completo
7. **[FEITO]** Segmentos + compaction (manifesto crash-safe) → base do LSM-tree
8. **[PRÓXIMO]** Concorrência: lock (`RwLock`) → depois MVCC (sequence numbers, snapshot isolation)
9. API HTTP / linguagem de query mínima

## Garantias a documentar no README final
Atomicidade, isolamento e durabilidade real. Declarar explicitamente, como um banco de
verdade: o que sobrevive a quê, qual o nível de isolamento dos leitores concorrentes, e como
o crash recovery foi testado (ex.: matar o processo com SIGKILL no meio de escritas).

---

## Definição de "pronto" do projeto 1 (o nível que impressiona recrutador)
O projeto não termina quando o código funciona — termina quando demonstra **como eu
trabalho**. O revisor não procura o tema mais difícil; procura evidência de que eu meço,
testo, lido com falha, documento decisões e termino o que começo. Os quatro entregáveis que
provam isso:

1. **Benchmarks publicados.** Medir throughput (escritas/s, leituras/s) e latência, e
   publicar os números (ex.: comparar com SQLite ou Redis). Mostra que eu meço, não chuto.
2. **Teste de crash recovery demonstrado.** Não basta dizer que sobrevive a crash — provar
   com um teste reproduzível que mata o processo (SIGKILL) no meio de escritas e verifica
   que nenhum dado confirmado se perdeu. É o entregável que mais separa este projeto.
3. **README em inglês** explicando **decisões e trade-offs**: por que LSM e não B-Tree, por
   que append-only, write/read amplification, quais garantias o engine oferece e quais não.
   Em inglês porque amplia o alcance pra recrutadores.
4. **CI rodando**: build + testes + lint (clippy) + fmt automatizados a cada push. Mostra
   disciplina de engenharia, não só de código.

> [!important]
> Quando o Claude sugerir o próximo passo, deve manter esses quatro entregáveis no radar —
> não me deixar declarar o projeto "pronto" sem eles. Profundidade na execução é o que
> contrata; é mais valioso terminar isto no nível obsessivo do que partir cedo pro próximo.

## Horizonte futuro (NÃO trabalhar nisso agora — só contexto)
Depois que o projeto 1 estiver impecável (os 4 entregáveis acima feitos), o próximo degrau
que faz revisor de sistemas levantar a sobrancelha é **coordenação distribuída**: implementar
o **Raft** (algoritmo de consenso, coração de etcd e Kafka moderno) ou replicação com
tolerância a partição de rede. O DDIA cobre a teoria (cap. 5 "Replication", cap. 9
"Consistency and Consensus"). É genuinamente difícil e poucos júniores/mids conseguem.

> [!warning]
> Isto é o degrau **seguinte**, não um atalho. A regra é **profundidade sobre dificuldade**:
> não trocar a execução rigorosa do projeto 1 por um tema mais exótico inacabado. Um projeto
> respondido com profundidade (mede? testa? documenta? termina?) impressiona mais que vários
> projetos exóticos pela metade. Se o Claude me vir querendo pular pro Raft antes do projeto 1
> estar no nível "pronto" acima, me lembrar disto.

---

## Livro de referência — DDIA (Designing Data-Intensive Applications, Kleppmann)
Estou lendo este livro em paralelo. Ao ensinar um conceito que o livro cobre, referencie o
**capítulo e a seção pelo nome** — **não invente número de página**: páginas variam por
edição (1ª/2ª, impressa/ebook, EN/PT) e o Claude não as conhece de forma confiável. Se não
tiver certeza da página, cite a seção. **Se eu disser uma página específica, confie em mim;
você não.**

Mapa dos capítulos relevantes:
- **Cap. 3 — Storage and Retrieval**: a espinha dorsal deste projeto.
  - "Hash Indexes" / Bitcask → nosso modelo atual (log + índice em memória).
  - "SSTables and LSM-Trees" → degraus de segmentos/compaction.
  - "B-Trees" → alternativa que NÃO seguimos, mas bom contraste (updates in-place vs append).
  - Comparação LSM vs B-Tree, write/read amplification → discutir no README final.
- **Cap. 7 — Transactions**: isolamento, atomicidade.
  - níveis de isolamento, "Snapshot Isolation" → embasa o degrau de MVCC.
- **Cap. 1 — Reliability, Scalability, Maintainability**: vocabulário das garantias que vou
  documentar.

Quando eu citar um conceito do livro, pode aprofundar e conectar com o que estou
implementando.

---

## Anotações de estudo (formato obrigatório)
- Toda explicação/conceito ensinado vira um `.md` em `notas/` (gitignored, uso no Obsidian).
- Um arquivo por sessão, em ordem (`01-...`, `02-...`). Nome em kebab-case.
- As notas são meu **guia de revisão futuro**: devo conseguir reler e reconstruir o raciocínio
  **sem reabrir o código**. Otimize para releitura, não para registro cru.

### Template fixo de cada nota (seguir sempre, nesta ordem)
1. **Cabeçalho**: título `# NN — tema`, link de volta pro índice (`[[00-indice]]`), link pra
   nota anterior.
2. **Objetivo da sessão**: 1-2 linhas — o que esta nota me ensina a fazer.
3. **Conceitos** (o coração): cada conceito em subseção própria. Para cada um:
   - explicação no meu nível (intermediário), com analogia quando ajudar;
   - quando vier do DDIA, citar capítulo/seção pelo nome (ver seção do livro acima).
4. **Especificação das funções**: assinatura + o que faz em ordem + decisões/edge cases.
   NÃO incluir a implementação completa (eu escrevo). Trechos de ≤3 linhas para sintaxe ok.
5. **Pegadinhas de Rust** que apareceram: o erro, o sintoma do compilador, o fix.
6. **Bugs que cometi** (se houver): o erro, por que aconteceu, a lição.
7. **Estado ao fim**: o que ficou funcionando, a **garantia real atual** (honesto sobre crash
   vs restart) e o **próximo passo** explícito.

### Regras de qualidade das notas
- Usar **callouts do Obsidian** para os "aha" em vez de emojis soltos: `> [!note]`,
  `> [!warning]`, `> [!tip]`, `> [!important]`. Renderizam como caixas coloridas e ficam
  muito mais escaneáveis numa revisão.
- **Tabelas** para comparações (tipos, formas de `self`, formato de retorno, etc.).
- Blocos de código com a linguagem marcada (` ```rust `, ` ```toml `).
- Toda garantia de durabilidade declarada DEVE dizer se é a **restart limpo** ou a **crash**.
- Ao fim da sessão, **atualizar `00-indice.md`** (marcar o degrau, ajustar "onde paramos") e
  o bloco "Onde paramos" deste CLAUDE.md.