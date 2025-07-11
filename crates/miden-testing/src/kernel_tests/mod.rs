use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::String,
    sync::Arc,
    vec::Vec,
};

use anyhow::Context;
use assert_matches::assert_matches;
use miden_lib::{
    note::{create_p2id_note, create_p2ide_note},
    transaction::TransactionKernel,
    utils::word_to_masm_push_string,
};
use miden_objects::{
    Felt, FieldElement, MIN_PROOF_SECURITY_LEVEL, Word,
    account::{
        Account, AccountBuilder, AccountComponent, AccountId, AccountStorage,
        delta::LexicographicWord,
    },
    assembly::diagnostics::{IntoDiagnostic, NamedSource, WrapErr, miette},
    asset::{Asset, AssetVault, FungibleAsset, NonFungibleAsset},
    note::{
        Note, NoteAssets, NoteExecutionHint, NoteExecutionMode, NoteHeader, NoteId, NoteInputs,
        NoteMetadata, NoteRecipient, NoteScript, NoteTag, NoteType,
    },
    testing::{
        account_component::{AccountMockComponent, IncrNonceAuthComponent},
        account_id::{
            ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET, ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1,
            ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2, ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_3,
            ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
            ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_UPDATABLE_CODE, ACCOUNT_ID_SENDER,
        },
        constants::{
            CONSUMED_ASSET_1_AMOUNT, CONSUMED_ASSET_3_AMOUNT, FUNGIBLE_ASSET_AMOUNT,
            NON_FUNGIBLE_ASSET_DATA, NON_FUNGIBLE_ASSET_DATA_2,
        },
        note::{DEFAULT_NOTE_CODE, NoteBuilder},
        storage::{STORAGE_INDEX_0, STORAGE_INDEX_2},
    },
    transaction::{OutputNote, ProvenTransaction, TransactionScript},
};
use miden_tx::{
    LocalTransactionProver, NoteAccountExecution, NoteConsumptionChecker, ProvingOptions,
    TransactionExecutor, TransactionExecutorError, TransactionHost, TransactionMastStore,
    TransactionProver, TransactionVerifier, host::ScriptMastForestStore,
};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;
use vm_processor::{
    AdviceInputs, Digest, ExecutionError, MemAdviceProvider, ONE,
    crypto::RpoRandomCoin,
    utils::{Deserializable, Serializable},
};

use crate::{Auth, MockChain, TransactionContextBuilder, TxContextInput, utils::create_p2any_note};

mod batch;
mod block;
mod tx;

// TESTS
// ================================================================================================

#[test]
fn transaction_executor_witness() -> miette::Result<()> {
    // Creates a mockchain with an account and a note that it can consume
    let tx_context = {
        let mut mock_chain = MockChain::new();
        let account = mock_chain.add_pending_existing_wallet(crate::Auth::BasicAuth, vec![]);
        let p2id_note = mock_chain
            .add_pending_p2id_note(
                ACCOUNT_ID_SENDER.try_into().unwrap(),
                account.id(),
                &[FungibleAsset::mock(100)],
                NoteType::Public,
            )
            .unwrap();
        mock_chain.prove_next_block().unwrap();

        mock_chain
            .build_tx_context(TxContextInput::AccountId(account.id()), &[], &[p2id_note])
            .unwrap()
            .build()
            .unwrap()
    };

    let source_manager = tx_context.source_manager();
    let executed_transaction = tx_context.execute().into_diagnostic()?;

    let tx_inputs = executed_transaction.tx_inputs();
    let tx_args = executed_transaction.tx_args();

    let scripts_mast_store = ScriptMastForestStore::new(
        tx_args.tx_script(),
        tx_inputs.input_notes().iter().map(|n| n.note().script()),
    );

    // use the witness to execute the transaction again
    let (stack_inputs, advice_inputs) = TransactionKernel::prepare_inputs(
        tx_inputs,
        tx_args,
        Some(executed_transaction.advice_witness().clone()),
    );

    let mem_advice_provider = MemAdviceProvider::from(advice_inputs.into_inner());

    // load account/note/tx_script MAST to the mast_store
    let mast_store = Arc::new(TransactionMastStore::new());
    mast_store.load_account_code(tx_inputs.account().code());

    let mut host: TransactionHost<MemAdviceProvider> = TransactionHost::new(
        &tx_inputs.account().into(),
        mem_advice_provider,
        mast_store.as_ref(),
        scripts_mast_store,
        None,
        BTreeSet::new(),
    )
    .unwrap();
    let result = vm_processor::execute(
        &TransactionKernel::main(),
        stack_inputs,
        &mut host,
        Default::default(),
        source_manager,
    )?;

    let (advice_provider, _, output_notes, _signatures, _tx_progress) = host.into_parts();
    let (_, map, _) = advice_provider.into_parts();
    let tx_outputs = TransactionKernel::from_transaction_parts(
        result.stack_outputs(),
        &map.into(),
        output_notes,
    )
    .unwrap();

    assert_eq!(
        executed_transaction.final_account().commitment(),
        tx_outputs.account.commitment()
    );
    assert_eq!(executed_transaction.output_notes(), &tx_outputs.output_notes);

    Ok(())
}

#[test]
fn executed_transaction_account_delta_new() -> anyhow::Result<()> {
    let account_assets = AssetVault::mock().assets().collect::<Vec<Asset>>();

    let account = AccountBuilder::new(ChaCha20Rng::from_os_rng().random())
        .with_auth_component(Auth::IncrNonce)
        .with_component(AccountMockComponent::new_with_slots(
            TransactionKernel::testing_assembler(),
            AccountStorage::mock_storage_slots(),
        )?)
        .with_assets(account_assets)
        .build_existing()?;

    // updated storage
    let updated_slot_value = [Felt::new(7), Felt::new(9), Felt::new(11), Felt::new(13)];

    // updated storage map
    let updated_map_key = [Felt::new(14), Felt::new(15), Felt::new(16), Felt::new(17)];
    let updated_map_value = [Felt::new(18), Felt::new(19), Felt::new(20), Felt::new(21)];

    // removed assets
    let removed_asset_1 = FungibleAsset::mock(FUNGIBLE_ASSET_AMOUNT / 2);
    let removed_asset_2 = Asset::Fungible(
        FungibleAsset::new(
            ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2.try_into().expect("id is valid"),
            FUNGIBLE_ASSET_AMOUNT,
        )
        .expect("asset is valid"),
    );
    let removed_asset_3 = NonFungibleAsset::mock(&NON_FUNGIBLE_ASSET_DATA);
    let removed_assets = [removed_asset_1, removed_asset_2, removed_asset_3];

    let tag1 =
        NoteTag::from_account_id(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE.try_into()?);
    let tag2 = NoteTag::for_local_use_case(0, 0)?;
    let tag3 = NoteTag::for_local_use_case(0, 0)?;
    let tags = [tag1, tag2, tag3];

    let aux_array = [Felt::new(27), Felt::new(28), Felt::new(29)];

    let note_types = [NoteType::Private; 3];

    tag1.validate(NoteType::Private)
        .expect("note tag 1 should support private notes");
    tag2.validate(NoteType::Private)
        .expect("note tag 2 should support private notes");
    tag3.validate(NoteType::Private)
        .expect("note tag 3 should support private notes");

    let execution_hint_1 = Felt::from(NoteExecutionHint::always());
    let execution_hint_2 = Felt::from(NoteExecutionHint::none());
    let execution_hint_3 = Felt::from(NoteExecutionHint::on_block_slot(1, 1, 1));
    let hints = [execution_hint_1, execution_hint_2, execution_hint_3];

    let mut send_asset_script = String::new();
    for i in 0..3 {
        send_asset_script.push_str(&format!(
            "
            ### note {i}
            # prepare the stack for a new note creation
            push.0.1.2.3           # recipient
            push.{EXECUTION_HINT}  # note_execution_hint
            push.{NOTETYPE}        # note_type
            push.{aux}             # aux
            push.{tag}             # tag
            # => [tag, aux, note_type, execution_hint, RECIPIENT]

            # pad the stack before calling the `create_note`
            padw padw swapdw
            # => [tag, aux, note_type, execution_hint, RECIPIENT, pad(8)]

            # create the note
            call.tx::create_note
            # => [note_idx, pad(15)]

            # move an asset to the created note to partially deplete fungible asset balance
            swapw dropw push.{REMOVED_ASSET}
            call.::miden::contracts::wallets::basic::move_asset_to_note
            # => [ASSET, note_idx, pad(11)]

            # clear the stack
            dropw dropw dropw dropw
        ",
            EXECUTION_HINT = hints[i],
            NOTETYPE = note_types[i] as u8,
            aux = aux_array[i],
            tag = tags[i],
            REMOVED_ASSET = word_to_masm_push_string(&Word::from(removed_assets[i]))
        ));
    }

    let tx_script_src = format!(
        "\
        use.test::account
        use.miden::tx

        ## TRANSACTION SCRIPT
        ## ========================================================================================
        begin
            ## Update account storage item
            ## ------------------------------------------------------------------------------------
            # push a new value for the storage slot onto the stack             
            push.{UPDATED_SLOT_VALUE}
            # => [13, 11, 9, 7]

            # get the index of account storage slot
            push.{STORAGE_INDEX_0}
            # => [idx, 13, 11, 9, 7]
            # update the storage value
            call.account::set_item dropw
            # => []

            ## Update account storage map
            ## ------------------------------------------------------------------------------------
            # push a new VALUE for the storage map onto the stack             
            push.{UPDATED_MAP_VALUE}
            # => [18, 19, 20, 21]

            # push a new KEY for the storage map onto the stack
            push.{UPDATED_MAP_KEY}
            # => [14, 15, 16, 17, 18, 19, 20, 21]

            # get the index of account storage slot
            push.{STORAGE_INDEX_2}
            # => [idx, 14, 15, 16, 17, 18, 19, 20, 21]

            # update the storage value
            call.account::set_map_item dropw dropw dropw
            # => []

            ## Send some assets from the account vault
            ## ------------------------------------------------------------------------------------
            {send_asset_script}

            dropw dropw dropw dropw
        end
    ",
        UPDATED_SLOT_VALUE = word_to_masm_push_string(&Word::from(updated_slot_value)),
        UPDATED_MAP_VALUE = word_to_masm_push_string(&Word::from(updated_map_value)),
        UPDATED_MAP_KEY = word_to_masm_push_string(&Word::from(updated_map_key)),
    );

    let tx_script = TransactionScript::compile(
        tx_script_src,
        TransactionKernel::testing_assembler_with_mock_account(),
    )?;

    // Create the input note that carries the assets that we will assert later
    let input_note = {
        let faucet_id_1 = AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1)?;
        let faucet_id_3 = AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_3)?;

        let fungible_asset_1: Asset =
            FungibleAsset::new(faucet_id_1, CONSUMED_ASSET_1_AMOUNT)?.into();
        let fungible_asset_3: Asset =
            FungibleAsset::new(faucet_id_3, CONSUMED_ASSET_3_AMOUNT)?.into();
        let nonfungible_asset_1: Asset = NonFungibleAsset::mock(&NON_FUNGIBLE_ASSET_DATA_2);

        create_p2any_note(account.id(), &[fungible_asset_1, fungible_asset_3, nonfungible_asset_1])
    };

    let tx_context = TransactionContextBuilder::new(account)
        .extend_input_notes(vec![input_note.clone()])
        .tx_script(tx_script)
        .build()?;

    // Storing assets that will be added to assert correctness later
    let added_assets = input_note.assets().iter().cloned().collect::<Vec<_>>();

    // expected delta
    // --------------------------------------------------------------------------------------------
    // execute the transaction and get the witness
    let executed_transaction = tx_context.execute()?;

    // nonce delta
    // --------------------------------------------------------------------------------------------

    assert_eq!(executed_transaction.account_delta().nonce_delta(), Felt::new(1));

    // storage delta
    // --------------------------------------------------------------------------------------------
    // We expect one updated item and one updated map
    assert_eq!(executed_transaction.account_delta().storage().values().len(), 1);
    assert_eq!(
        executed_transaction.account_delta().storage().values().get(&STORAGE_INDEX_0),
        Some(&updated_slot_value)
    );

    assert_eq!(executed_transaction.account_delta().storage().maps().len(), 1);
    let map_delta = executed_transaction
        .account_delta()
        .storage()
        .maps()
        .get(&STORAGE_INDEX_2)
        .context("failed to get expected value from storage map")?
        .entries();
    assert_eq!(
        *map_delta.get(&LexicographicWord::new(Digest::from(updated_map_key))).unwrap(),
        updated_map_value
    );

    // vault delta
    // --------------------------------------------------------------------------------------------
    // assert that added assets are tracked
    assert!(
        executed_transaction
            .account_delta()
            .vault()
            .added_assets()
            .all(|x| added_assets.contains(&x))
    );
    assert_eq!(
        added_assets.len(),
        executed_transaction.account_delta().vault().added_assets().count()
    );

    // assert that removed assets are tracked
    assert!(
        executed_transaction
            .account_delta()
            .vault()
            .removed_assets()
            .all(|x| removed_assets.contains(&x))
    );
    assert_eq!(
        removed_assets.len(),
        executed_transaction.account_delta().vault().removed_assets().count()
    );
    Ok(())
}

#[test]
fn test_send_note_proc() -> miette::Result<()> {
    // removed assets
    let removed_asset_1 = FungibleAsset::mock(FUNGIBLE_ASSET_AMOUNT / 2);
    let removed_asset_2 = Asset::Fungible(
        FungibleAsset::new(
            ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2.try_into().expect("id is valid"),
            FUNGIBLE_ASSET_AMOUNT,
        )
        .expect("asset is valid"),
    );
    let removed_asset_3 = NonFungibleAsset::mock(&NON_FUNGIBLE_ASSET_DATA);

    let tag = NoteTag::from_account_id(
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE.try_into().unwrap(),
    );
    let aux = Felt::new(27);
    let note_type = NoteType::Private;

    tag.validate(note_type).expect("note tag should support private notes");

    // prepare the asset vector to be removed for each test variant
    let assets_matrix = vec![
        vec![],
        vec![removed_asset_1],
        vec![removed_asset_1, removed_asset_2],
        vec![removed_asset_1, removed_asset_2, removed_asset_3],
    ];

    for (idx, removed_assets) in assets_matrix.into_iter().enumerate() {
        // Prepare the string containing the procedures required for adding assets to the note.
        // Depending on the number of the assets to remove, the resulting string will be extended
        // with the corresponding number of procedure "blocks"
        let mut assets_to_remove = String::new();
        for asset in removed_assets.iter() {
            assets_to_remove.push_str(&format!(
                "\n
            # prepare the stack for the next call
            dropw

            # push the asset to be removed
            push.{ASSET}
            # => [ASSET, note_idx, GARBAGE(11)]

            call.wallet::move_asset_to_note
            # => [ASSET, note_idx, GARBAGE(11)]\n",
                ASSET = word_to_masm_push_string(&asset.into())
            ))
        }

        let tx_script_src = format!(
            "\
            use.miden::contracts::wallets::basic->wallet
            use.miden::tx
            use.test::account

            ## TRANSACTION SCRIPT
            ## ========================================================================================
            begin
                # prepare the values for note creation
                push.1.2.3.4      # recipient
                push.1            # note_execution_hint (NoteExecutionHint::Always)
                push.{note_type}  # note_type
                push.{aux}        # aux
                push.{tag}        # tag
                # => [tag, aux, note_type, RECIPIENT, ...]

                # pad the stack with zeros before calling the `create_note`.
                padw padw swapdw
                # => [tag, aux, execution_hint, note_type, RECIPIENT, pad(8) ...]

                call.tx::create_note
                # => [note_idx, GARBAGE(15)]

                movdn.4
                # => [GARBAGE(4), note_idx, GARBAGE(11)]

                {assets_to_remove}

                dropw dropw dropw dropw
            end
        ",
            note_type = note_type as u8,
        );

        let tx_script = TransactionScript::compile(
            tx_script_src,
            TransactionKernel::testing_assembler_with_mock_account(),
        )
        .unwrap();

        let tx_context = TransactionContextBuilder::with_existing_mock_account()
            .tx_script(tx_script)
            .build()
            .unwrap();

        // expected delta
        // --------------------------------------------------------------------------------------------
        // execute the transaction and get the witness
        let executed_transaction = tx_context
            .execute()
            .into_diagnostic()
            .wrap_err(format!("test failed in iteration {idx}"))?;

        // nonce delta
        // --------------------------------------------------------------------------------------------

        // nonce was incremented by 1
        assert_eq!(executed_transaction.account_delta().nonce_delta(), ONE);

        // vault delta
        // --------------------------------------------------------------------------------------------
        // assert that removed assets are tracked
        assert!(
            executed_transaction
                .account_delta()
                .vault()
                .removed_assets()
                .all(|x| removed_assets.contains(&x))
        );
        assert_eq!(
            removed_assets.len(),
            executed_transaction.account_delta().vault().removed_assets().count()
        );
    }

    Ok(())
}

#[test]
fn executed_transaction_output_notes() -> anyhow::Result<()> {
    let assembler = TransactionKernel::testing_assembler();
    let auth_component = IncrNonceAuthComponent::new(assembler.clone())?;

    let executor_account = Account::mock(
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_UPDATABLE_CODE,
        Felt::ONE,
        auth_component,
        assembler,
    );
    let account_id = executor_account.id();

    // removed assets
    let removed_asset_1 = FungibleAsset::mock(FUNGIBLE_ASSET_AMOUNT / 2);
    let removed_asset_2 = FungibleAsset::mock(FUNGIBLE_ASSET_AMOUNT / 2);

    let combined_asset = Asset::Fungible(
        FungibleAsset::new(
            ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET.try_into().expect("id is valid"),
            FUNGIBLE_ASSET_AMOUNT,
        )
        .expect("asset is valid"),
    );
    let removed_asset_3 = NonFungibleAsset::mock(&NON_FUNGIBLE_ASSET_DATA);
    let removed_asset_4 = Asset::Fungible(
        FungibleAsset::new(
            ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2.try_into().expect("id is valid"),
            FUNGIBLE_ASSET_AMOUNT / 2,
        )
        .expect("asset is valid"),
    );

    let tag1 = NoteTag::from_account_id(
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE.try_into().unwrap(),
    );
    let tag2 = NoteTag::for_public_use_case(0, 0, NoteExecutionMode::Local).unwrap();
    let tag3 = NoteTag::for_public_use_case(0, 0, NoteExecutionMode::Local).unwrap();
    let aux1 = Felt::new(27);
    let aux2 = Felt::new(28);
    let aux3 = Felt::new(29);

    let note_type1 = NoteType::Private;
    let note_type2 = NoteType::Public;
    let note_type3 = NoteType::Public;

    tag1.validate(note_type1).expect("note tag 1 should support private notes");
    tag2.validate(note_type2).expect("note tag 2 should support public notes");
    tag3.validate(note_type3).expect("note tag 3 should support public notes");

    // In this test we create 3 notes. Note 1 is private, Note 2 is public and Note 3 is public
    // without assets.

    // Create the expected output note for Note 2 which is public
    let serial_num_2 = Word::from([Felt::new(1), Felt::new(2), Felt::new(3), Felt::new(4)]);
    let note_script_2 =
        NoteScript::compile(DEFAULT_NOTE_CODE, TransactionKernel::testing_assembler())?;
    let inputs_2 = NoteInputs::new(vec![ONE])?;
    let metadata_2 =
        NoteMetadata::new(account_id, note_type2, tag2, NoteExecutionHint::none(), aux2)?;
    let vault_2 = NoteAssets::new(vec![removed_asset_3, removed_asset_4])?;
    let recipient_2 = NoteRecipient::new(serial_num_2, note_script_2, inputs_2);
    let expected_output_note_2 = Note::new(vault_2, metadata_2, recipient_2);

    // Create the expected output note for Note 3 which is public
    let serial_num_3 = Word::from([Felt::new(5), Felt::new(6), Felt::new(7), Felt::new(8)]);
    let note_script_3 =
        NoteScript::compile(DEFAULT_NOTE_CODE, TransactionKernel::testing_assembler())?;
    let inputs_3 = NoteInputs::new(vec![ONE, Felt::new(2)])?;
    let metadata_3 = NoteMetadata::new(
        account_id,
        note_type3,
        tag3,
        NoteExecutionHint::on_block_slot(1, 2, 3),
        aux3,
    )?;
    let vault_3 = NoteAssets::new(vec![])?;
    let recipient_3 = NoteRecipient::new(serial_num_3, note_script_3, inputs_3);
    let expected_output_note_3 = Note::new(vault_3, metadata_3, recipient_3);

    let tx_script_src = format!(
        "\
        use.miden::contracts::wallets::basic->wallet
        use.miden::tx
        use.test::account

        # Inputs:  [tag, aux, note_type, execution_hint, RECIPIENT]
        # Outputs: [note_idx]
        proc.create_note
            # pad the stack before the call to prevent accidental modification of the deeper stack
            # elements
            padw padw swapdw
            # => [tag, aux, execution_hint, note_type, RECIPIENT, pad(8)]

            call.tx::create_note
            # => [note_idx, pad(15)]

            # remove excess PADs from the stack
            swapdw dropw dropw movdn.7 dropw drop drop drop
            # => [note_idx]
        end

        # Inputs:  [ASSET, note_idx]
        # Outputs: [ASSET, note_idx]
        proc.move_asset_to_note
            # pad the stack before call
            push.0.0.0 movdn.7 movdn.7 movdn.7 padw padw swapdw
            # => [ASSET, note_idx, pad(11)]

            call.wallet::move_asset_to_note
            # => [ASSET, note_idx, pad(11)]

            # remove excess PADs from the stack
            swapdw dropw dropw swapw movdn.7 drop drop drop
            # => [ASSET, note_idx]
        end

        ## TRANSACTION SCRIPT
        ## ========================================================================================
        begin
            ## Send some assets from the account vault
            ## ------------------------------------------------------------------------------------
            # partially deplete fungible asset balance
            push.0.1.2.3                        # recipient
            push.{EXECUTION_HINT_1}             # note execution hint
            push.{NOTETYPE1}                    # note_type
            push.{aux1}                         # aux
            push.{tag1}                         # tag
            exec.create_note
            # => [note_idx]
            
            push.{REMOVED_ASSET_1}              # asset_1
            # => [ASSET, note_idx]

            exec.move_asset_to_note dropw
            # => [note_idx]

            push.{REMOVED_ASSET_2}              # asset_2
            exec.move_asset_to_note dropw drop
            # => []

            # send non-fungible asset
            push.{RECIPIENT2}                   # recipient
            push.{EXECUTION_HINT_2}             # note execution hint
            push.{NOTETYPE2}                    # note_type
            push.{aux2}                         # aux
            push.{tag2}                         # tag
            exec.create_note
            # => [note_idx]

            push.{REMOVED_ASSET_3}              # asset_3
            exec.move_asset_to_note dropw
            # => [note_idx]

            push.{REMOVED_ASSET_4}              # asset_4
            exec.move_asset_to_note dropw drop
            # => []

            # create a public note without assets
            push.{RECIPIENT3}                   # recipient
            push.{EXECUTION_HINT_3}             # note execution hint
            push.{NOTETYPE3}                    # note_type
            push.{aux3}                         # aux
            push.{tag3}                         # tag
            exec.create_note drop
            # => []
        end
    ",
        REMOVED_ASSET_1 = word_to_masm_push_string(&Word::from(removed_asset_1)),
        REMOVED_ASSET_2 = word_to_masm_push_string(&Word::from(removed_asset_2)),
        REMOVED_ASSET_3 = word_to_masm_push_string(&Word::from(removed_asset_3)),
        REMOVED_ASSET_4 = word_to_masm_push_string(&Word::from(removed_asset_4)),
        RECIPIENT2 =
            word_to_masm_push_string(&Word::from(expected_output_note_2.recipient().digest())),
        RECIPIENT3 =
            word_to_masm_push_string(&Word::from(expected_output_note_3.recipient().digest())),
        NOTETYPE1 = note_type1 as u8,
        NOTETYPE2 = note_type2 as u8,
        NOTETYPE3 = note_type3 as u8,
        EXECUTION_HINT_1 = Felt::from(NoteExecutionHint::always()),
        EXECUTION_HINT_2 = Felt::from(NoteExecutionHint::none()),
        EXECUTION_HINT_3 = Felt::from(NoteExecutionHint::on_block_slot(11, 22, 33)),
    );

    let tx_script = TransactionScript::compile(
        tx_script_src,
        TransactionKernel::testing_assembler_with_mock_account().with_debug_mode(true),
    )?;

    // expected delta
    // --------------------------------------------------------------------------------------------
    // execute the transaction and get the witness

    let tx_context = TransactionContextBuilder::new(executor_account)
        .tx_script(tx_script)
        .extend_expected_output_notes(vec![
            OutputNote::Full(expected_output_note_2.clone()),
            OutputNote::Full(expected_output_note_3.clone()),
        ])
        .build()?;

    let executed_transaction = tx_context.execute()?;

    // output notes
    // --------------------------------------------------------------------------------------------
    let output_notes = executed_transaction.output_notes();

    // check the total number of notes
    assert_eq!(output_notes.num_notes(), 3);

    // assert that the expected output note 1 is present
    let resulting_output_note_1 = executed_transaction.output_notes().get_note(0);

    let expected_recipient_1 =
        Digest::from([Felt::new(0), Felt::new(1), Felt::new(2), Felt::new(3)]);
    let expected_note_assets_1 = NoteAssets::new(vec![combined_asset])?;
    let expected_note_id_1 = NoteId::new(expected_recipient_1, expected_note_assets_1.commitment());
    assert_eq!(resulting_output_note_1.id(), expected_note_id_1);

    // assert that the expected output note 2 is present
    let resulting_output_note_2 = executed_transaction.output_notes().get_note(1);

    let expected_note_id_2 = expected_output_note_2.id();
    let expected_note_metadata_2 = expected_output_note_2.metadata();
    assert_eq!(
        NoteHeader::from(resulting_output_note_2),
        NoteHeader::new(expected_note_id_2, *expected_note_metadata_2)
    );

    // assert that the expected output note 3 is present and has no assets
    let resulting_output_note_3 = executed_transaction.output_notes().get_note(2);

    assert_eq!(expected_output_note_3.id(), resulting_output_note_3.id());
    assert_eq!(expected_output_note_3.assets(), resulting_output_note_3.assets().unwrap());

    // make sure that the number of note inputs remains the same
    let resulting_note_2_recipient =
        resulting_output_note_2.recipient().expect("output note 2 is not full");
    assert_eq!(
        resulting_note_2_recipient.inputs().num_values(),
        expected_output_note_2.inputs().num_values()
    );

    let resulting_note_3_recipient =
        resulting_output_note_3.recipient().expect("output note 3 is not full");
    assert_eq!(
        resulting_note_3_recipient.inputs().num_values(),
        expected_output_note_3.inputs().num_values()
    );

    Ok(())
}

#[allow(clippy::arc_with_non_send_sync)]
#[test]
fn prove_witness_and_verify() -> anyhow::Result<()> {
    let tx_context = {
        let account = Account::mock(
            ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_UPDATABLE_CODE,
            Felt::ONE,
            Auth::IncrNonce,
            TransactionKernel::testing_assembler(),
        );
        let input_note =
            create_p2any_note(ACCOUNT_ID_SENDER.try_into().unwrap(), &[FungibleAsset::mock(100)]);
        TransactionContextBuilder::new(account)
            .extend_input_notes(vec![input_note])
            .build()?
    };

    let source_manager = tx_context.source_manager();
    let account_id = tx_context.tx_inputs().account().id();

    let block_ref = tx_context.tx_inputs().block_header().block_num();
    let notes = tx_context.tx_inputs().input_notes().clone();
    let tx_args = tx_context.tx_args().clone();
    let executor = TransactionExecutor::new(&tx_context, None);
    let executed_transaction = executor.execute_transaction(
        account_id,
        block_ref,
        notes,
        tx_args,
        Arc::clone(&source_manager),
    )?;
    let executed_transaction_id = executed_transaction.id();

    let proof_options = ProvingOptions::default();
    let prover = LocalTransactionProver::new(proof_options);
    let proven_transaction = prover.prove(executed_transaction.into())?;

    assert_eq!(proven_transaction.id(), executed_transaction_id);

    let serialized_transaction = proven_transaction.to_bytes();
    let proven_transaction = ProvenTransaction::read_from_bytes(&serialized_transaction)?;
    let verifier = TransactionVerifier::new(MIN_PROOF_SECURITY_LEVEL);
    assert!(verifier.verify(&proven_transaction).is_ok());

    Ok(())
}

// TEST TRANSACTION SCRIPT
// ================================================================================================

#[test]
fn test_tx_script_inputs() -> anyhow::Result<()> {
    let tx_script_input_key = [Felt::new(9999), Felt::new(8888), Felt::new(9999), Felt::new(8888)];
    let tx_script_input_value = [Felt::new(9), Felt::new(8), Felt::new(7), Felt::new(6)];
    let tx_script_src = format!(
        "
        use.miden::account

        begin
            # push the tx script input key onto the stack
            push.{key}

            # load the tx script input value from the map and read it onto the stack
            adv.push_mapval adv_loadw

            # assert that the value is correct
            push.{value} assert_eqw
        end
        ",
        key = word_to_masm_push_string(&tx_script_input_key),
        value = word_to_masm_push_string(&tx_script_input_value)
    );

    let tx_script =
        TransactionScript::compile(tx_script_src, TransactionKernel::testing_assembler()).unwrap();

    let tx_context = TransactionContextBuilder::with_existing_mock_account()
        .tx_script(tx_script)
        .extend_advice_map([(tx_script_input_key, tx_script_input_value.into())])
        .build()?;

    tx_context.execute().context("failed to execute transaction")?;

    Ok(())
}

#[test]
fn test_tx_script_args() -> anyhow::Result<()> {
    let tx_script_arg = [Felt::new(1), Felt::new(2), Felt::new(3), Felt::new(4)];

    let tx_script_src = r#"
        use.miden::account

        begin
            # => [TX_SCRIPT_ARG]
            # `TX_SCRIPT_ARG` value is a user provided word, which could be used during the
            # transaction execution. In this example it is a `[1, 2, 3, 4]` word.

            # assert the correctness of the argument
            dupw push.1.2.3.4 assert_eqw.err="provided transaction argument doesn't match the expected one"
            # => [TX_SCRIPT_ARG]

            # since we provided an advice map entry with the transaction script argument as a key, 
            # we can obtain the value of this entry
            adv.push_mapval adv_push.4
            # => [[map_entry_values], TX_SCRIPT_ARG]

            # assert the correctness of the map entry values
            push.5.6.7.8 assert_eqw.err="obtained advice map value doesn't match the expected one"
        end"#;

    let tx_script =
        TransactionScript::compile(tx_script_src, TransactionKernel::testing_assembler())
            .context("failed to compile transaction script")?;

    // create an advice inputs containing the entry which could be accessed using the provided
    // transaction script argument
    let advice_inputs = AdviceInputs::default().with_map([(
        Digest::new(tx_script_arg),
        vec![Felt::new(5), Felt::new(6), Felt::new(7), Felt::new(8)],
    )]);

    let tx_context = TransactionContextBuilder::with_existing_mock_account()
        .tx_script(tx_script)
        .extend_advice_inputs(advice_inputs)
        .tx_script_arg(tx_script_arg)
        .build()?;

    tx_context.execute()?;

    Ok(())
}

/// Tests that an account can call code in a custom library when loading that library into the
/// executor.
///
/// The call chain and dependency graph in this test is:
/// `tx script -> account code -> external library`
#[test]
fn transaction_executor_account_code_using_custom_library() -> miette::Result<()> {
    const EXTERNAL_LIBRARY_CODE: &str = r#"
      use.miden::account

      export.external_setter
        push.2.3.4.5
        push.0
        exec.account::set_item
        dropw dropw
      end"#;

    const ACCOUNT_COMPONENT_CODE: &str = "
      use.external_library::external_module

      export.custom_setter
        exec.external_module::external_setter
      end";

    let external_library_source =
        NamedSource::new("external_library::external_module", EXTERNAL_LIBRARY_CODE);
    let external_library =
        TransactionKernel::assembler().assemble_library([external_library_source])?;

    let mut assembler = TransactionKernel::testing_assembler_with_mock_account();
    assembler.add_vendored_library(&external_library)?;

    let account_component_source =
        NamedSource::new("account_component::account_module", ACCOUNT_COMPONENT_CODE);
    let account_component_lib =
        assembler.clone().assemble_library([account_component_source]).unwrap();

    let tx_script_src = "\
          use.account_component::account_module

          begin
            call.account_module::custom_setter
          end";

    let account_component =
        AccountComponent::new(account_component_lib.clone(), AccountStorage::mock_storage_slots())
            .into_diagnostic()?
            .with_supports_all_types();

    // Build an existing account with nonce 1.
    let native_account = AccountBuilder::new(ChaCha20Rng::from_os_rng().random())
        .with_auth_component(Auth::IncrNonce)
        .with_component(account_component)
        .build_existing()
        .into_diagnostic()?;

    let tx_script = TransactionScript::compile(
        tx_script_src,
        // Add the account component library since the transaction script is calling the account's
        // procedure.
        assembler.with_library(&account_component_lib)?,
    )
    .into_diagnostic()?;

    let tx_context = TransactionContextBuilder::new(native_account.clone())
        .tx_script(tx_script)
        .build()
        .unwrap();

    let executed_tx = tx_context.execute().into_diagnostic()?;

    // Account's initial nonce of 1 should have been incremented by 1.
    assert_eq!(executed_tx.account_delta().nonce_delta(), Felt::new(1));

    // Make sure that account storage has been updated as per the tx script call.
    assert_eq!(
        *executed_tx.account_delta().storage().values(),
        BTreeMap::from([(0, [Felt::new(2), Felt::new(3), Felt::new(4), Felt::new(5)])]),
    );
    Ok(())
}

#[allow(clippy::arc_with_non_send_sync)]
#[test]
fn test_execute_program() -> anyhow::Result<()> {
    let test_module_source = "
        export.foo
            push.3.4
            add
            swapw dropw
        end
    ";

    let source = NamedSource::new("test::module_1", test_module_source);
    let assembler = TransactionKernel::assembler();
    let source_manager = assembler.source_manager();
    let assembler = assembler
        .with_module(source)
        .map_err(|_| anyhow::anyhow!("adding source module"))?;

    let source = "
    use.test::module_1
    use.std::sys
    
    begin
        push.1.2
        call.module_1::foo
        exec.sys::truncate_stack
    end
    ";

    let tx_script = TransactionScript::compile(source, assembler)?;

    let tx_context = TransactionContextBuilder::with_existing_mock_account()
        .tx_script(tx_script.clone())
        .build()?;
    let account_id = tx_context.account().id();
    let block_ref = tx_context.tx_inputs().block_header().block_num();
    let advice_inputs = tx_context.tx_args().advice_inputs().clone();

    let executor = TransactionExecutor::new(&tx_context, None);

    let stack_outputs = executor.execute_tx_view_script(
        account_id,
        block_ref,
        tx_script,
        advice_inputs,
        Vec::default(),
        source_manager,
    )?;

    assert_eq!(stack_outputs[..3], [Felt::new(7), Felt::new(2), ONE]);

    Ok(())
}

#[allow(clippy::arc_with_non_send_sync)]
#[test]
fn test_check_note_consumability() -> anyhow::Result<()> {
    // Success (well known notes)
    // --------------------------------------------------------------------------------------------
    let p2id_note = create_p2id_note(
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE.try_into().unwrap(),
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_UPDATABLE_CODE.try_into().unwrap(),
        vec![FungibleAsset::mock(10)],
        NoteType::Public,
        Default::default(),
        &mut RpoRandomCoin::new([ONE, Felt::new(2), Felt::new(3), Felt::new(4)]),
    )?;

    let p2ide_note = create_p2ide_note(
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE.try_into().unwrap(),
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_UPDATABLE_CODE.try_into().unwrap(),
        vec![FungibleAsset::mock(10)],
        None,
        None,
        NoteType::Public,
        Default::default(),
        &mut RpoRandomCoin::new([ONE, Felt::new(2), Felt::new(3), Felt::new(4)]),
    )?;

    let tx_context = TransactionContextBuilder::with_existing_mock_account()
        .extend_input_notes(vec![p2id_note, p2ide_note])
        .build()?;
    let source_manager = tx_context.source_manager();

    let input_notes = tx_context.input_notes().clone();
    let target_account_id = tx_context.account().id();
    let block_ref = tx_context.tx_inputs().block_header().block_num();
    let tx_args = tx_context.tx_args().clone();

    let executor: TransactionExecutor = TransactionExecutor::new(&tx_context, None).with_tracing();
    let notes_checker = NoteConsumptionChecker::new(&executor);

    let execution_check_result = notes_checker.check_notes_consumability(
        target_account_id,
        block_ref,
        input_notes,
        tx_args,
        source_manager,
    )?;
    assert_matches!(execution_check_result, NoteAccountExecution::Success);

    // Success (custom notes)
    // --------------------------------------------------------------------------------------------
    let tx_context = {
        let account = Account::mock(
            ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_UPDATABLE_CODE,
            Felt::ONE,
            Auth::IncrNonce,
            TransactionKernel::testing_assembler(),
        );
        let input_note =
            create_p2any_note(ACCOUNT_ID_SENDER.try_into().unwrap(), &[FungibleAsset::mock(100)]);
        TransactionContextBuilder::new(account)
            .extend_input_notes(vec![input_note])
            .build()?
    };
    let source_manager = tx_context.source_manager();

    let input_notes = tx_context.input_notes().clone();
    let account_id = tx_context.account().id();
    let block_ref = tx_context.tx_inputs().block_header().block_num();
    let tx_args = tx_context.tx_args().clone();

    let executor: TransactionExecutor = TransactionExecutor::new(&tx_context, None).with_tracing();
    let notes_checker = NoteConsumptionChecker::new(&executor);

    let execution_check_result = notes_checker.check_notes_consumability(
        account_id,
        block_ref,
        input_notes,
        tx_args,
        source_manager,
    )?;
    assert_matches!(execution_check_result, NoteAccountExecution::Success);

    // Failure
    // --------------------------------------------------------------------------------------------
    let mut mock_chain = MockChain::new();
    let account = mock_chain.add_pending_existing_wallet(crate::Auth::BasicAuth, vec![]);

    let sender = AccountId::try_from(ACCOUNT_ID_SENDER).unwrap();

    let failing_note_1 = NoteBuilder::new(
        sender,
        ChaCha20Rng::from_seed(ChaCha20Rng::from_seed([0_u8; 32]).random()),
    )
    .code("begin push.1 drop push.0 div end")
    .build(&TransactionKernel::testing_assembler())?;

    let failing_note_2 = NoteBuilder::new(
        sender,
        ChaCha20Rng::from_seed(ChaCha20Rng::from_seed([0_u8; 32]).random()),
    )
    .code("begin push.2 drop push.0 div end")
    .build(&TransactionKernel::testing_assembler())?;

    let successful_note_1 = create_p2id_note(
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE.try_into().unwrap(),
        account.id(),
        vec![FungibleAsset::mock(10)],
        NoteType::Public,
        Default::default(),
        &mut RpoRandomCoin::new([ONE, Felt::new(2), Felt::new(3), Felt::new(4)]),
    )?;

    let successful_note_2 = create_p2id_note(
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE.try_into().unwrap(),
        account.id(),
        vec![FungibleAsset::mock(145)],
        NoteType::Public,
        Default::default(),
        &mut RpoRandomCoin::new([ONE, Felt::new(2), Felt::new(3), Felt::new(4)]),
    )?;

    let tx_context = mock_chain
        .build_tx_context(
            TxContextInput::Account(account),
            &[],
            &[
                successful_note_2.clone(),
                successful_note_1.clone(),
                failing_note_2.clone(),
                failing_note_1,
            ],
        )?
        .build()?;
    let source_manager = tx_context.source_manager();

    let input_notes = tx_context.input_notes().clone();
    let account_id = tx_context.account().id();
    let block_ref = tx_context.tx_inputs().block_header().block_num();
    let tx_args = tx_context.tx_args().clone();

    let executor: TransactionExecutor = TransactionExecutor::new(&tx_context, None).with_tracing();
    let notes_checker = NoteConsumptionChecker::new(&executor);

    let execution_check_result = notes_checker.check_notes_consumability(
        account_id,
        block_ref,
        input_notes,
        tx_args,
        source_manager,
    )?;

    assert_matches!(execution_check_result, NoteAccountExecution::Failure {
        failed_note_id,
        successful_notes,
        error: Some(e)} => {
            assert_eq!(failed_note_id, failing_note_2.id());
            assert_eq!(successful_notes, [successful_note_2.id(),successful_note_1.id()].to_vec());
            assert_matches!(e, TransactionExecutorError::TransactionProgramExecutionFailed(
              ExecutionError::DivideByZero { .. }
            ));
        }
    );
    Ok(())
}
