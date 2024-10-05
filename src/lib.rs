#[cfg(test)]
mod tests {
    use fuel_asm::{op, Instruction, RegId};
    use fuels::{
        accounts::Account,
        core::constants::WORD_SIZE,
        macros::setup_program_test,
        tx::Receipt,
        types::{
            transaction_builders::{Blob, BlobTransactionBuilder},
            Bits256, U256,
        },
    };

    fn extract_data_offset(binary: &[u8]) -> usize {
        let data_offset: [u8; 8] = binary[8..16].try_into().expect("checked above");

        u64::from_be_bytes(data_offset) as usize
    }

    fn transform_into_configurable_loader(binary: Vec<u8>, blob_id: &[u8; 32]) -> Vec<u8> {
        // The final code is going to have this structure (if the data section is non-empty):
        // 1. loader instructions
        // 2. blob id
        // 3. length_of_data_section
        // 4. the data_section (updated with configurables as needed)
        const BLOB_ID_SIZE: u16 = 32;
        const REG_ADDRESS_OF_DATA_AFTER_CODE: u8 = 0x10;
        const REG_START_OF_LOADED_CODE: u8 = 0x11;
        const REG_GENERAL_USE: u8 = 0x12;
        const REG_START_OF_DATA_SECTION: u8 = 0x13;
        let get_instructions = |num_of_instructions| {
            // There are 3 main steps:
            // 1. Load the blob content into memory
            // 2. Load the data section right after the blob
            // 3. Jump to the beginning of the memory where the blob was loaded
            [
                // 1. Load the blob content into memory
                // Find the start of the hardcoded blob ID, which is located after the loader code ends.
                op::move_(REG_ADDRESS_OF_DATA_AFTER_CODE, RegId::PC),
                // hold the address of the blob ID.
                op::addi(
                    REG_ADDRESS_OF_DATA_AFTER_CODE,
                    REG_ADDRESS_OF_DATA_AFTER_CODE,
                    num_of_instructions * Instruction::SIZE as u16,
                ),
                // The code is going to be loaded from the current value of SP onwards, save
                // the location into REG_START_OF_LOADED_CODE so we can jump into it at the end.
                op::move_(REG_START_OF_LOADED_CODE, RegId::SP),
                // REG_GENERAL_USE to hold the size of the blob.
                op::bsiz(REG_GENERAL_USE, REG_ADDRESS_OF_DATA_AFTER_CODE),
                op::move_(0x16, REG_GENERAL_USE),
                // Push the blob contents onto the stack.
                op::ldc(REG_ADDRESS_OF_DATA_AFTER_CODE, 0, REG_GENERAL_USE, 1),
                // Move on to the data section length
                op::addi(
                    REG_ADDRESS_OF_DATA_AFTER_CODE,
                    REG_ADDRESS_OF_DATA_AFTER_CODE,
                    BLOB_ID_SIZE,
                ),
                // load the size of the data section into REG_GENERAL_USE
                op::lw(REG_GENERAL_USE, REG_ADDRESS_OF_DATA_AFTER_CODE, 0),
                // after we have read the length of the data section, we move the pointer to the actual
                // data by skipping WORD_SIZE B.
                op::addi(
                    REG_ADDRESS_OF_DATA_AFTER_CODE,
                    REG_ADDRESS_OF_DATA_AFTER_CODE,
                    WORD_SIZE as u16,
                ),
                // extend the stack
                op::cfe(REG_GENERAL_USE),
                // move to the start of the newly allocated stack
                op::sub(REG_START_OF_DATA_SECTION, RegId::SP, REG_GENERAL_USE),
                // load the data section onto the stack
                op::mcp(
                    REG_START_OF_DATA_SECTION,
                    REG_ADDRESS_OF_DATA_AFTER_CODE,
                    REG_GENERAL_USE,
                ),
                op::add(0x16, 0x16, REG_GENERAL_USE),
                op::logd(RegId::ZERO, RegId::ZERO, REG_START_OF_LOADED_CODE, 0x16),
                // Jump into the memory where the contract is loaded.
                // What follows is called _jmp_mem by the sway compiler.
                // Subtract the address contained in IS because jmp will add it back.
                op::sub(
                    REG_START_OF_LOADED_CODE,
                    REG_START_OF_LOADED_CODE,
                    RegId::IS,
                ),
                // jmp will multiply by 4, so we need to divide to cancel that out.
                op::divi(REG_START_OF_LOADED_CODE, REG_START_OF_LOADED_CODE, 4),
                // Jump to the start of the contract we loaded.
                op::jmp(REG_START_OF_LOADED_CODE),
            ]
        };

        let offset = extract_data_offset(&binary);

        let data_section = binary[offset..].to_vec();

        let num_of_instructions = u16::try_from(get_instructions(0).len())
            .expect("to never have more than u16::MAX instructions");

        let instruction_bytes = get_instructions(num_of_instructions)
            .into_iter()
            .flat_map(|instruction| instruction.to_bytes());

        let blob_bytes = blob_id.iter().copied();

        let data_section_len: u64 = u64::try_from(data_section.len())
            .expect("to never have more than u64::MAX data section length");

        instruction_bytes
            .chain(blob_bytes)
            .chain(data_section_len.to_be_bytes())
            .chain(data_section)
            .collect()
    }

    #[tokio::test]
    async fn test_name() {
        setup_program_test!(
            Wallets("wallet"),
            Abigen(Script(name = "MyScript", project = "script")),
        );

        let provider = wallet.provider().unwrap().clone();

        let binary = std::fs::read("./script/out/release/script.bin").unwrap();

        let data_section_offset = extract_data_offset(&binary);

        let without_data_section = binary[..data_section_offset].to_vec();
        let blob = Blob::new(without_data_section);
        let blob_id = blob.id();

        let mut tb = BlobTransactionBuilder::default().with_blob(blob);

        wallet.adjust_for_fee(&mut tb, 0).await.unwrap();
        wallet.add_witnesses(&mut tb).unwrap();

        let tx = tb.build(provider.clone()).await.unwrap();
        provider
            .send_transaction_and_await_commit(tx)
            .await
            .unwrap();

        let temp_file = tempfile::tempdir().unwrap();
        let loader_file = temp_file.path().join("loader.bin");
        std::fs::write(
            &loader_file,
            transform_into_configurable_loader(binary, &blob_id),
        )
        .unwrap();
        let my_script = MyScript::new(wallet.clone(), loader_file.to_str().unwrap());

        let response = my_script.main().call().await.unwrap();

        let extract_log_data = |receipts: &Vec<Receipt>| {
            for receipt in receipts {
                if let Receipt::LogData { data, .. } = receipt {
                    let bytes = hex::encode(data.as_ref().unwrap());
                    return Some(bytes);
                }
            }
            None
        };

        let receipts = response.receipts;

        let log_data = extract_log_data(&receipts).expect("log data not found");

        let raw_code = hex::encode(std::fs::read("./script/out/release/script.bin").unwrap());

        assert_eq!(log_data, raw_code);

        let expected_value = (
            true,
            8,
            16,
            32,
            63,
            U256::from(8),
            Bits256([1; 32]),
            "fuel".try_into().unwrap(),
            (8, true),
        );

        pretty_assertions::assert_eq!(response.value, expected_value);
    }
}
