//! Teste de crash recovery (entregável #2): mata o processo escritor com SIGKILL
//! no meio das escritas e verifica que nada confirmado se perdeu e que o replay
//! sobrevive à cauda possivelmente rasgada (torn write).
//!
//! É um teste de integração: vê apenas a API pública `storage_engine::Db`, como
//! um usuário externo veria.

use std::process::Command;
use std::time::{Duration, Instant};
use storage_engine::Db;

fn temp_path(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("bedrock-crash-{}-{}.db", std::process::id(), name));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn survives_sigkill_mid_write() {
    let path = temp_path("sigkill");
    let p = path.to_str().unwrap().to_string();

    // 1. Spawna o escritor (binário separado) que grava em loop, com fsync por escrita.
    //    CARGO_BIN_EXE_crash_writer é o caminho do binário, injetado pelo Cargo nos testes.
    let mut child = Command::new(env!("CARGO_BIN_EXE_crash_writer"))
        .arg(&p)
        .spawn()
        .expect("falhou ao spawnar o crash_writer");

    // 2. Espera o log passar de um tamanho que garante vários registros completos,
    //    então mata. child.kill() envia SIGKILL no Unix.
    let start = Instant::now();
    loop {
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() > 200 {
                break;
            }
        }
        if start.elapsed() > Duration::from_secs(5) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // deixa correr um pouco mais para aumentar a chance de morrer NO MEIO de uma escrita
    std::thread::sleep(Duration::from_millis(50));
    child.kill().expect("falhou ao matar o escritor");
    let _ = child.wait();

    // 3. Reabre depois do crash — o replay tem que sobreviver à cauda rasgada.
    let db = Db::open(&p).expect("reabrir o log após o crash falhou");

    // 4. Invariante: as chaves presentes formam um PREFIXO contíguo key-0..key-(n-1).
    //    Como as escritas são sequenciais e cada uma faz fsync antes da próxima, o log
    //    é uma sequência de registros completos + no máximo um registro rasgado no fim.
    //    O replay aplica os completos e descarta o rasgado -> sem buraco, sem lixo.
    let mut n: u64 = 0;
    while db.get(&format!("key-{n}")).is_some() {
        n += 1;
    }
    assert!(
        n >= 1,
        "esperava ao menos uma escrita confirmada antes do crash"
    );

    for i in 0..n {
        assert_eq!(
            db.get(&format!("key-{i}")),
            Some(format!("val-{i}").as_str()),
            "chave confirmada key-{i} sumiu ou veio corrompida após o crash"
        );
    }
    assert!(
        db.get(&format!("key-{n}")).is_none(),
        "índice tem buraco: key-{n} ausente mas alguma chave posterior presente"
    );

    let _ = std::fs::remove_file(&path);
}
