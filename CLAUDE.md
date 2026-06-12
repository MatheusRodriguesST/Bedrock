# Bedrock — guia do projeto (para o Claude e para mim)

## Ideia principal
Bedrock é um **storage engine / mini banco de dados durável**, escrito em **Rust**, feito como
projeto de portfólio. O objetivo é **aprender sistemas implementando**: storage em disco,
durabilidade, recuperação pós-crash, índices e concorrência.

Garantias que o projeto pretende oferecer e **documentar**: atomicidade, isolamento, e
durabilidade real (dados sobrevivem a restart e a crash).

Workspace Cargo:
- `crates/storage-engine` — o motor (binário, onde mora a lógica)
- `crates/server` — futura API HTTP / query (ainda vazio)

## Decisões de design já firmadas
- **NÃO usar JSON** como formato de armazenamento. O ponto é controlar o formato em disco.
- Formato v1 do log: **textual, append-only** — `SET\tchave\tvalor\n` e `DEL\tchave\n`.
  Depois migra para binário com tamanho-prefixado.
- Modelo de base: **log append-only + índice em memória** (estilo Bitcask, cap. 3 do livro
  *Designing Data-Intensive Applications*).

## Roadmap em degraus (cada um resolve a limitação do anterior)
1. **[FEITO]** `struct Db` + `open` (abre/cria o log, índice em memória vazio)
2. `set` / `get` — escrever no log e ler do índice
3. `delete` via tombstone
4. Replay do log no `open` (reconstruir o índice no boot) → durabilidade a restart
5. `fsync` + checksum por registro → durabilidade a crash (vira WAL de verdade)
6. Índice de offsets (guarda chave→posição, não o valor) → estilo Bitcask completo
7. Segmentos + compaction → base do LSM-tree
8. Concorrência: lock → depois MVCC
9. API HTTP / linguagem de query mínima

## Como o Claude deve trabalhar comigo (IMPORTANTE)
- Eu sou **aluno**; o Claude é **instrutor**. Nível de Rust: intermediário.
- **NÃO escrever o código de implementação por mim.** Explicar conceitos, especificar as
  funções (assinatura + o que cada uma faz), apontar decisões de design e edge cases, e me
  deixar implementar. Pode escrever scaffolding/testes não-centrais e revisar meu código.
- Sempre ser **honesto sobre as garantias reais** do código atual (ex.: "isto sobrevive a
  restart limpo, mas ainda não a crash sem fsync").

## Anotações de estudo
- Toda explicação/conceito ensinado deve ser salvo como **`.md` na pasta `notas/`**
  (gitignored — uso no Obsidian para estudar).
- Um arquivo por tema/sessão, em ordem (`01-...`, `02-...`).
- No fim de cada sessão, registrar **onde paramos e o que já foi feito**, para a próxima
  sessão ter contexto e eu saber exatamente o próximo passo.
