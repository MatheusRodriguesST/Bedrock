//! Binário auxiliar do teste de crash recovery (`tests/crash_recovery.rs`).
//!
//! Recebe o caminho do log por argumento, abre o banco e escreve chaves
//! sequenciais (`key-0`, `key-1`, …) num loop infinito — cada `set` faz `fsync`,
//! então cada escrita é durável antes da próxima começar. O processo é morto com
//! SIGKILL pelo teste no meio das escritas; não há saída limpa, e é esse o ponto.

use storage_engine::Db;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("uso: crash_writer <caminho_do_log>");
    let mut db = Db::open(&path).expect("falhou ao abrir o log");

    let mut i: u64 = 0;
    loop {
        db.set(format!("key-{i}"), format!("val-{i}"))
            .expect("falhou ao escrever");
        i += 1;
    }
}
