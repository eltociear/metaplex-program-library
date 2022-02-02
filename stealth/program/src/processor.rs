
use crate::{
    instruction::*,
    state::*,
    error::*,
    pod::*,
    transfer_proof::{Verifiable, TransferProof},
    equality_proof::*,
    transcript::TranscriptProtocol,
    zk_token_elgamal,
    ID,
};

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    msg,
    program_pack::Pack,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction,
    sysvar::{Sysvar},
};

use std::convert::TryInto;

pub fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    input: &[u8],
) -> ProgramResult {
    match decode_instruction_type(input)? {
        StealthInstruction::ConfigureMetadata => {
            msg!("ConfigureMetadata!");
            process_configure_metadata(
                accounts,
                decode_instruction_data::<ConfigureMetadataData>(input)?
            )
        }
        StealthInstruction::InitTransfer => {
            msg!("InitTransfer!");
            process_init_transfer(
                accounts,
            )
        }
        StealthInstruction::FiniTransfer => {
            msg!("FiniTransfer!");
            process_fini_transfer(
                accounts,
            )
        }
        StealthInstruction::TransferChunk => {
            msg!("TransferChunk!");
            process_transfer_chunk(
                accounts,
                decode_instruction_data::<TransferChunkData>(input)?
            )
        }
        StealthInstruction::TransferChunkSlow => {
            msg!("TransferChunkSlow!");
            process_transfer_chunk_slow(
                accounts,
                decode_instruction_data::<TransferChunkSlowData>(input)?
            )
        }
        StealthInstruction::PublishElgamalPubkey => {
            msg!("PublishElgamalPubkey!");
            process_publish_elgamal_pubkey(
                accounts,
                decode_instruction_data::<zk_token_elgamal::pod::ElGamalPubkey>(input)?
            )
        }
        StealthInstruction::CloseElgamalPubkey => {
            msg!("CloseElgamalPubkey!");
            process_close_elgamal_pubkey(
                accounts,
            )
        }
    }
}

// TODO: Result instead of assuming overflow
fn scale_creator_shares(
    stealth_key: &Pubkey,
    metadata: &mpl_token_metadata::state::Metadata,
) -> Option<Vec<mpl_token_metadata::state::Creator>> {
    let mut new_creators = vec![];
    if let Some(creators) = &metadata.data.creators {
        let current_seller_bp = u64::from(metadata.data.seller_fee_basis_points);
        let mut remaining_share: u8 = 100;
        for creator in creators {
            let current_creator_bp = current_seller_bp
                .checked_mul(u64::from(creator.share))?
                .checked_div(100)?;
            let next_creator_share: u8 = match current_creator_bp.checked_div(100)?.try_into() {
                Ok(v) => v,
                Err(_) => {
                    msg!("Internal error: share recalculation failed");
                    return None;
                }
            };
            remaining_share = remaining_share
                .checked_sub(next_creator_share)?;
            new_creators.push(mpl_token_metadata::state::Creator {
                share: next_creator_share,
                ..*creator
            });
        }
        new_creators.push(mpl_token_metadata::state::Creator {
            address: *stealth_key,
            verified: false,
            share: remaining_share,
        });
    }
    Some(new_creators)
}

fn reassign_royalties<'info>(
    metadata_program_info: &AccountInfo<'info>,
    stealth_info: &AccountInfo<'info>,
    metadata_info: &AccountInfo<'info>,
    metadata_update_authority_info: &AccountInfo<'info>,
    metadata: &mpl_token_metadata::state::Metadata,
    signer_seeds: &[&[&[u8]]],
    _account_info_iter: &mut std::slice::Iter<AccountInfo<'info>>,
) -> ProgramResult {
    if *metadata_program_info.key != mpl_token_metadata::ID {
        msg!("Mismatched metadata program");
        return Err(ProgramError::InvalidArgument);
    }

    // make the PDA a 'creator' so that it receives a portion of the fees and bump seller fees to
    // 100%
    let new_creators = scale_creator_shares(&stealth_info.key, &metadata)
        .ok_or::<ProgramError>(StealthError::Overflow.into())?;
    invoke(
        &mpl_token_metadata::instruction::update_metadata_accounts(
            *metadata_program_info.key,
            *metadata_info.key,
            *metadata_update_authority_info.key,
            None, // new update auth
            Some(mpl_token_metadata::state::Data {
                seller_fee_basis_points: 10000,
                creators: Some(new_creators),
                ..metadata.data.clone()
            }),
            None, // primary sale happened
        ),
        &[
            metadata_program_info.clone(),
            metadata_info.clone(),
            metadata_update_authority_info.clone(),
        ],
    )?;

    invoke_signed(
        &mpl_token_metadata::instruction::sign_metadata(
            *metadata_program_info.key,
            *metadata_info.key,
            *stealth_info.key,
        ),
        &[
            metadata_program_info.clone(),
            metadata_info.clone(),
            stealth_info.clone(),
        ],
        signer_seeds,
    )?;

    Ok(())
}

fn reassign_mint_and_freeze<'info>(
    token_program_info: &AccountInfo<'info>,
    stealth_info: &AccountInfo<'info>,
    mint_info: &AccountInfo<'info>,
    mint_authority_info: &AccountInfo<'info>,
    metadata: &mpl_token_metadata::state::Metadata,
    signer_seeds: &[&[&[u8]]],
    account_info_iter: &mut std::slice::Iter<AccountInfo<'info>>,
) -> ProgramResult {
    if *token_program_info.key != spl_token::ID {
        msg!("Mismatched token program");
        return Err(ProgramError::InvalidArgument);
    }

    if metadata.mint != *mint_info.key {
        msg!("Mismatched mint");
        return Err(StealthError::InvalidMintInfo.into());
    }

    let mint = spl_token::state::Mint::unpack_from_slice(&mint_info.try_borrow_data()?)?;

    if mint.decimals != 0 {
        msg!("Decimals not zero");
        return Err(StealthError::InvalidMintInfo.into());
    }

    if mint.supply != 1 {
        msg!("Supply is not 1");
        return Err(StealthError::InvalidMintInfo.into());
    }

    if solana_program::program_option::COption::Some(metadata.update_authority) != mint.mint_authority {
        msg!("Mint authority and metadata authority are different");
        return Err(StealthError::InvalidUpdateAuthority.into());
    }

    // reassign mint and freeze auth
    let accounts = &[
        mint_authority_info.clone(),
        mint_info.clone(),
        token_program_info.clone(),
        stealth_info.clone(),
    ];
    invoke(
        &spl_token::instruction::set_authority(
            token_program_info.key,
            mint_info.key,
            Some(stealth_info.key),
            spl_token::instruction::AuthorityType::MintTokens,
            mint_authority_info.key,
            &[],
        ).unwrap(),
        accounts,
    )?;

    // currently freeze authority cannot be re-enabled but if it's changed in token program
    // later...
    invoke(
        &spl_token::instruction::set_authority(
            token_program_info.key,
            mint_info.key,
            Some(&stealth_info.key),
            spl_token::instruction::AuthorityType::FreezeAccount,
            mint_authority_info.key,
            &[],
        ).unwrap(),
        accounts,
    )?;

    let token_account_info = next_account_info(account_info_iter)?;
    let token_account = spl_token::state::Account::unpack_from_slice(
        &token_account_info.try_borrow_data()?)?;

    if token_account.mint != *mint_info.key {
        msg!("Mismatched token account mint");
        return Err(StealthError::InvalidTokenAccountInfo.into());
    }

    // equals old mint auth
    if token_account.owner != *mint_authority_info.key {
        msg!("Mismatched token account owner");
        return Err(StealthError::InvalidTokenAccountInfo.into());
    }

    if token_account.amount != 1 {
        msg!("Mismatched token account amount");
        return Err(StealthError::InvalidTokenAccountInfo.into());
    }

    invoke_signed(
        &spl_token::instruction::freeze_account(
            token_program_info.key,
            token_account_info.key,
            mint_info.key,
            mint_authority_info.key,
            &[],
        ).unwrap(),
        &[
            token_program_info.clone(),
            token_account_info.clone(),
            mint_info.clone(),
            mint_authority_info.clone(),
        ],
        signer_seeds
    )?;

    Ok(())
}

fn process_configure_metadata(
    accounts: &[AccountInfo],
    data: &ConfigureMetadataData
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let payer_info = next_account_info(account_info_iter)?;
    let mint_info = next_account_info(account_info_iter)?;
    let metadata_info = next_account_info(account_info_iter)?;
    let metadata_update_authority_info = next_account_info(account_info_iter)?;
    let stealth_info = next_account_info(account_info_iter)?;
    let oversight_program_info = next_account_info(account_info_iter)?;
    let system_program_info = next_account_info(account_info_iter)?;
    let rent_sysvar_info = next_account_info(account_info_iter)?;

    if !payer_info.is_signer {
        msg!("Payer is not a signer");
        return Err(ProgramError::InvalidArgument);
    }

    if !metadata_update_authority_info.is_signer {
        msg!("Metadata update authority is not a signer");
        return Err(ProgramError::InvalidArgument);
    }
    validate_account_owner(mint_info, &spl_token::ID)?;
    validate_account_owner(metadata_info, &mpl_token_metadata::ID)?;

    // check metadata matches mint
    let metadata_seeds = &[
        mpl_token_metadata::state::PREFIX.as_bytes(),
        mpl_token_metadata::ID.as_ref(),
        mint_info.key.as_ref(),
    ];
    let (metadata_key, _metadata_bump_seed) =
        Pubkey::find_program_address(metadata_seeds, &mpl_token_metadata::ID);

    if metadata_key != *metadata_info.key {
        msg!("Invalid metadata key");
        return Err(StealthError::InvalidMetadataKey.into());
    }


    // check that metadata authority matches and that metadata is mutable (adding Stealth
    // and not acting on a limited edition). TODO?
    let metadata = mpl_token_metadata::state::Metadata::from_account_info(metadata_info)?;

    let authority_pubkey = metadata.update_authority;

    if authority_pubkey != *metadata_update_authority_info.key {
        msg!("Invalid metadata update authority");
        return Err(StealthError::InvalidUpdateAuthority.into());
    }

    if !metadata.is_mutable {
        msg!("Metadata is immutable");
        return Err(StealthError::MetadataIsImmutable.into());
    }


    // check that Stealth matches mint
    let stealth_seeds = &[
        PREFIX.as_bytes(),
        mint_info.key.as_ref(),
    ];
    let (stealth_key, stealth_bump_seed) =
        Pubkey::find_program_address(stealth_seeds, &ID);

    if stealth_key != *stealth_info.key {
        msg!("Invalid stealth key");
        return Err(StealthError::InvalidStealthKey.into());
    }

    let mint_info_key = mint_info.key;
    let signer_seeds : &[&[&[u8]]] = &[
        &[
            PREFIX.as_bytes(),
            mint_info_key.as_ref(),
            &[stealth_bump_seed],
        ],
    ];

    // create and initialize PDA
    let rent = &Rent::from_account_info(rent_sysvar_info)?;
    invoke_signed(
        &system_instruction::create_account(
            payer_info.key,
            stealth_info.key,
            rent.minimum_balance(StealthAccount::get_packed_len()).max(1),
            StealthAccount::get_packed_len() as u64,
            &ID,
        ),
        &[
            payer_info.clone(),
            stealth_info.clone(),
            system_program_info.clone(),
        ],
        signer_seeds,
    )?;

    let mut stealth = StealthAccount::from_account_info(
        &stealth_info, &ID, Key::Uninitialized)?.into_mut();

    stealth.key = Key::StealthAccountV1;
    stealth.mint = *mint_info.key;
    stealth.wallet_pk = *payer_info.key;
    stealth.elgamal_pk = data.elgamal_pk;
    stealth.encrypted_cipher_key = data.encrypted_cipher_key;
    stealth.uri = data.uri;
    stealth.method = data.method;
    stealth.bump_seed = stealth_bump_seed;

    drop(stealth);

    match data.method {
        OversightMethod::Royalties => {
            reassign_royalties(
                oversight_program_info,
                stealth_info,
                metadata_info,
                metadata_update_authority_info,
                &metadata,
                signer_seeds,
                account_info_iter,
            )
        }
        OversightMethod::Freeze => {
            reassign_mint_and_freeze(
                oversight_program_info,
                stealth_info,
                mint_info,
                metadata_update_authority_info,
                &metadata,
                signer_seeds,
                account_info_iter,
            )
        }
        OversightMethod::None => {
            Ok(())
        }
        _ => {
            msg!("Invalid OversightMethod");
            Err(ProgramError::InvalidArgument)
        }
    }?;

    Ok(())
}

// TODO: since creating filling the transfer buffer (even just sending the instruction and if they
// fail somehow or are snooped by someone along the way) fully allows the dest keypair to decrypt
// so it needs to be some handshake process i think...
//
// can this be a separate program?
//
// - bid is marked accepted by the seller
//     - seller commits some portion to escrow (10%?)
//     - bid funds are locked for period X
// - before X elapses, the seller does the full transfer and the program releases all funds to the
//   seller once fini is accepted + nft has been transferred
// - after X, buyer can show key has not yet been transfered and claim their funds back along with
//   the seller escrow
//
// i think this means that only 1 sale can happen at a time? which does seem correct since their is
// only 1 and this 'atomic' operation is kind of split

fn process_init_transfer(
    accounts: &[AccountInfo],
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let payer_info = next_account_info(account_info_iter)?;
    let mint_info = next_account_info(account_info_iter)?;
    let token_account_info = next_account_info(account_info_iter)?;
    let stealth_info = next_account_info(account_info_iter)?;
    let recipient_info = next_account_info(account_info_iter)?;
    let recipient_elgamal_info = next_account_info(account_info_iter)?;
    let transfer_buffer_info = next_account_info(account_info_iter)?;
    let system_program_info = next_account_info(account_info_iter)?;
    let rent_sysvar_info = next_account_info(account_info_iter)?;

    if !payer_info.is_signer {
        return Err(ProgramError::InvalidArgument);
    }
    validate_account_owner(mint_info, &spl_token::ID)?;
    validate_account_owner(token_account_info, &spl_token::ID)?;
    validate_account_owner(stealth_info, &ID)?;

    let token_account = spl_token::state::Account::unpack(
        &token_account_info.data.borrow())?;

    if token_account.mint != *mint_info.key {
        msg!("Mint mismatch");
        return Err(ProgramError::InvalidArgument);
    }

    if token_account.owner != *payer_info.key {
        msg!("Owner mismatch");
        return Err(ProgramError::InvalidArgument);
    }

    // TODO: this is a bit fucky since the nft token transfer should really happen at the same time
    // as the stealth transfer...
    if token_account.amount != 1 {
        msg!("Invalid amount");
        return Err(ProgramError::InvalidArgument);
    }


    // check that stealth matches mint
    let (stealth_key, _stealth_bump_seed) =
        get_stealth_address(mint_info.key);

    if stealth_key != *stealth_info.key {
        return Err(StealthError::InvalidStealthKey.into());
    }

    // deserialize to verify it exists...
    let stealth = StealthAccount::from_account_info(
        &stealth_info, &ID, Key::StealthAccountV1)?;

    // check that elgamal PDAs match
    let get_elgamal_pk = |
        wallet_info: &AccountInfo,
        elgamal_info: &AccountInfo,
    | -> Result<zk_token_elgamal::pod::ElGamalPubkey, ProgramError> {
        let (elgamal_pubkey_key, _elgamal_pubkey_bump_seed) =
            get_elgamal_pubkey_address(wallet_info.key, mint_info.key);

        if elgamal_pubkey_key != *elgamal_info.key {
            msg!("Invalid elgamal PDA");
            return Err(StealthError::InvalidElgamalPubkeyPDA.into());
        }

        let encryption_buffer = EncryptionKeyBuffer::from_account_info(
            &recipient_elgamal_info, &ID, Key::EncryptionKeyBufferV1)?;

        Ok(encryption_buffer.elgamal_pk)
    };

    let recipient_elgamal_pk = get_elgamal_pk(
        &recipient_info, &recipient_elgamal_info)?;


    // check and initialize the cipher key transfer buffer
    let (transfer_buffer_key, transfer_buffer_bump_seed) =
        get_transfer_buffer_address(recipient_info.key, mint_info.key);

    if transfer_buffer_key != *transfer_buffer_info.key {
        msg!("Invalid transfer buffer key");
        return Err(ProgramError::InvalidArgument);
    }

    let rent = &Rent::from_account_info(rent_sysvar_info)?;
    invoke_signed(
        &system_instruction::create_account(
            payer_info.key,
            transfer_buffer_info.key,
            rent.minimum_balance(CipherKeyTransferBuffer::get_packed_len()).max(1),
            CipherKeyTransferBuffer::get_packed_len() as u64,
            &ID,
        ),
        &[
            payer_info.clone(),
            transfer_buffer_info.clone(),
            system_program_info.clone(),
        ],
        &[
            &[
                TRANSFER.as_bytes(),
                recipient_info.key.as_ref(),
                mint_info.key.as_ref(),
                &[transfer_buffer_bump_seed],
            ],
        ],
    )?;

    let mut transfer_buffer = CipherKeyTransferBuffer::from_account_info(
        &transfer_buffer_info, &ID, Key::Uninitialized)?.into_mut();

    // low bits should be clear regardless...
    transfer_buffer.key = Key::CipherKeyTransferBufferV1;
    transfer_buffer.stealth_key = *stealth_info.key;
    transfer_buffer.wallet_pk = *recipient_info.key;
    transfer_buffer.elgamal_pk = recipient_elgamal_pk;

    match stealth.method {
        OversightMethod::Royalties => {
            let minimum_rent = rent.minimum_balance(
                StealthAccount::get_packed_len()).max(1);
            let paid_amount =
                stealth_info.lamports()
                .checked_sub(minimum_rent)
                .ok_or::<ProgramError>(StealthError::Overflow.into())?;
            if paid_amount != 0 {
                // transfer the seller's fee portion to the transfer buffer (which can be claimed by them)
                // TODO: expiration so buyer can reclaim if this doesn't happen
                let starting_lamports = transfer_buffer_info.lamports();
                **transfer_buffer_info.lamports.borrow_mut() = starting_lamports
                    .checked_add(paid_amount)
                    .ok_or::<ProgramError>(StealthError::Overflow.into())?;

                **stealth_info.lamports.borrow_mut() = minimum_rent;
            }
        }
        _ => {}
    }

    Ok(())
}

// TODO: this should be cheap and should be bundled with the actual NFT transfer
fn process_fini_transfer(
    accounts: &[AccountInfo],
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let authority_info = next_account_info(account_info_iter)?;
    let stealth_info = next_account_info(account_info_iter)?;
    let transfer_buffer_info = next_account_info(account_info_iter)?;
    let _system_program_info = next_account_info(account_info_iter)?;

    if !authority_info.is_signer {
        return Err(ProgramError::InvalidArgument);
    }

    // check that transfer buffer matches passed in arguments and that we have authority to do
    // the transfer
    //
    // TODO: should we have a nother check for nft ownership here?
    let transfer_buffer = CipherKeyTransferBuffer::from_account_info(
        &transfer_buffer_info, &ID, Key::CipherKeyTransferBufferV1)?;

    let mut stealth = StealthAccount::from_account_info(
        &stealth_info, &ID, Key::StealthAccountV1)?.into_mut();

    validate_transfer_buffer(
        &transfer_buffer,
        &stealth,
        authority_info.key,
        stealth_info.key,
    )?;

    if !bool::from(&transfer_buffer.updated) {
        msg!("Not all chunks set");
        return Err(ProgramError::InvalidArgument);
    }


    // write the new cipher text over

    stealth.wallet_pk = transfer_buffer.wallet_pk;
    stealth.elgamal_pk = transfer_buffer.elgamal_pk;
    stealth.encrypted_cipher_key = transfer_buffer.encrypted_cipher_key;

    let stealth_bump_seed = stealth.bump_seed;
    let stealth_method = stealth.method;
    drop(stealth);

    let close_transfer_buffer = || -> ProgramResult {
        let starting_lamports = authority_info.lamports();
        **authority_info.lamports.borrow_mut() = starting_lamports
            .checked_add(transfer_buffer_info.lamports())
            .ok_or::<ProgramError>(StealthError::Overflow.into())?;

        **transfer_buffer_info.lamports.borrow_mut() = 0;
        Ok(())
    };

    if account_info_iter.clone().count() == 0 {
        // no wrapped transfer
        if stealth_method == OversightMethod::Freeze {
            msg!("Must use fini_transfer with token accounts with freeze oversight");
            return Err(ProgramError::InvalidArgument);
        }
        close_transfer_buffer()?;
        return Ok(());
    }


    let mint_info = next_account_info(account_info_iter)?;
    let source_info = next_account_info(account_info_iter)?;
    let destination_info = next_account_info(account_info_iter)?;
    let token_program_info = next_account_info(account_info_iter)?;

    let mint_info_key = mint_info.key;
    let signer_seeds : &[&[&[u8]]] = &[
        &[
            PREFIX.as_bytes(),
            mint_info_key.as_ref(),
            &[stealth_bump_seed],
        ],
    ];

    if stealth_method == OversightMethod::Freeze {
        invoke_signed(
            &spl_token::instruction::thaw_account(
                token_program_info.key,
                source_info.key,
                mint_info.key,
                stealth_info.key,
                &[],
            ).unwrap(),
            &[
                token_program_info.clone(),
                source_info.clone(),
                mint_info.clone(),
                stealth_info.clone(),
            ],
            signer_seeds
        )?;
    }

    invoke(
        &spl_token::instruction::transfer(
            token_program_info.key,
            source_info.key,
            destination_info.key,
            authority_info.key,
            &[],
            1,
        ).unwrap(),
        &[
            token_program_info.clone(),
            source_info.clone(),
            destination_info.clone(),
            authority_info.clone(),
        ],
    )?;

    if stealth_method == OversightMethod::Freeze {
        invoke_signed(
            &spl_token::instruction::freeze_account(
                token_program_info.key,
                destination_info.key,
                mint_info.key,
                stealth_info.key,
                &[],
            ).unwrap(),
            &[
                token_program_info.clone(),
                destination_info.clone(),
                mint_info.clone(),
                stealth_info.clone(),
            ],
            signer_seeds
        )?;
    }

    close_transfer_buffer()?;

    Ok(())
}

fn process_transfer_chunk(
    accounts: &[AccountInfo],
    data: &TransferChunkData,
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let authority_info = next_account_info(account_info_iter)?;
    let stealth_info = next_account_info(account_info_iter)?;
    let transfer_buffer_info = next_account_info(account_info_iter)?;
    let _system_program_info = next_account_info(account_info_iter)?;

    if !authority_info.is_signer {
        return Err(ProgramError::InvalidArgument);
    }

    // check that transfer buffer matches passed in arguments and that we have authority to do
    // the transfer
    //
    // TODO: should we have a nother check for nft ownership here?
    let mut transfer_buffer = CipherKeyTransferBuffer::from_account_info(
        &transfer_buffer_info, &ID, Key::CipherKeyTransferBufferV1)?.into_mut();

    let stealth = StealthAccount::from_account_info(
        &stealth_info, &ID, Key::StealthAccountV1)?;

    validate_transfer_buffer(
        &transfer_buffer,
        &stealth,
        authority_info.key,
        stealth_info.key,
    )?;

    // check that this proof has matching pubkey fields and that we haven't already processed this
    // chunk
    if bool::from(&transfer_buffer.updated) {
        msg!("Chunk already updated");
        return Err(ProgramError::InvalidArgument);
    }

    let transfer = &data.transfer;
    if transfer.transfer_public_keys.src_pubkey != stealth.elgamal_pk {
        msg!("Source elgamal pubkey mismatch");
        return Err(ProgramError::InvalidArgument);
    }

    if transfer.src_cipher_key_chunk_ct != stealth.encrypted_cipher_key {
        msg!("Source cipher text mismatch");
        return Err(ProgramError::InvalidArgument);
    }

    if transfer.transfer_public_keys.dst_pubkey != transfer_buffer.elgamal_pk {
        msg!("Destination elgamal pubkey mismatch");
        return Err(ProgramError::InvalidArgument);
    }


    // actually verify the proof...
    // TODO: syscalls when available
    if transfer.verify().is_err() {
        return Err(StealthError::ProofVerificationError.into());
    }

    transfer_buffer.updated = true.into();
    transfer_buffer.encrypted_cipher_key = transfer.dst_cipher_key_chunk_ct;


    Ok(())
}

fn process_transfer_chunk_slow(
    accounts: &[AccountInfo],
    data: &TransferChunkSlowData,
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let authority_info = next_account_info(account_info_iter)?;
    let stealth_info = next_account_info(account_info_iter)?;
    let transfer_buffer_info = next_account_info(account_info_iter)?;
    let instruction_buffer_info = next_account_info(account_info_iter)?;
    let input_buffer_info = next_account_info(account_info_iter)?;
    let compute_buffer_info = next_account_info(account_info_iter)?;
    let _system_program_info = next_account_info(account_info_iter)?;

    if !authority_info.is_signer {
        return Err(ProgramError::InvalidArgument);
    }

    // check that transfer buffer matches passed in arguments and that we have authority to do
    // the transfer
    //
    // TODO: should we have a nother check for nft ownership here?
    let mut transfer_buffer = CipherKeyTransferBuffer::from_account_info(
        &transfer_buffer_info, &ID, Key::CipherKeyTransferBufferV1)?.into_mut();

    let stealth = StealthAccount::from_account_info(
        &stealth_info, &ID, Key::StealthAccountV1)?;

    validate_transfer_buffer(
        &transfer_buffer,
        &stealth,
        authority_info.key,
        stealth_info.key,
    )?;

    // check that this proof has matching pubkey fields and that we haven't already processed this
    // chunk
    if bool::from(&transfer_buffer.updated) {
        msg!("Chunk already updated");
        return Err(ProgramError::InvalidArgument);
    }

    let transfer = &data.transfer;
    if transfer.transfer_public_keys.src_pubkey != stealth.elgamal_pk {
        msg!("Source elgamal pubkey mismatch");
        return Err(ProgramError::InvalidArgument);
    }

    if transfer.src_cipher_key_chunk_ct != stealth.encrypted_cipher_key {
        msg!("Source cipher text mismatch");
        return Err(ProgramError::InvalidArgument);
    }

    if transfer.transfer_public_keys.dst_pubkey != transfer_buffer.elgamal_pk {
        msg!("Destination elgamal pubkey mismatch");
        return Err(ProgramError::InvalidArgument);
    }




    msg!("Verifying comopute inputs...");
    use curve25519_dalek_onchain::instruction as dalek;
    use std::borrow::Borrow;
    use borsh::BorshDeserialize;

    validate_account_owner(instruction_buffer_info, &curve25519_dalek_onchain::ID)?;
    validate_account_owner(input_buffer_info, &curve25519_dalek_onchain::ID)?;
    validate_account_owner(compute_buffer_info, &curve25519_dalek_onchain::ID)?;

    let conv_error = || -> ProgramError { StealthError::ProofVerificationError.into() };

    // check that the compute buffer points to the right things
    let compute_buffer_data = compute_buffer_info.try_borrow_data()?;
    let mut compute_buffer_ptr: &[u8] = compute_buffer_data.borrow();
    let compute_buffer_header = dalek::ComputeHeader::deserialize(&mut compute_buffer_ptr)?;
    if dalek::HEADER_SIZE < 128 {
        msg!("Header size seems too small");
        return Err(ProgramError::InvalidArgument);
    }
    if compute_buffer_header.authority != *authority_info.key {
        msg!("Invalid compute buffer authority");
        return Err(ProgramError::InvalidArgument);
    }
    if compute_buffer_header.instruction_buffer != *instruction_buffer_info.key {
        msg!("Mismatched instruction buffer");
        return Err(ProgramError::InvalidArgument);
    }
    if compute_buffer_header.input_buffer != *input_buffer_info.key {
        msg!("Mismatched input buffer");
        return Err(ProgramError::InvalidArgument);
    }
    let expected_count: u32 = DSL_INSTRUCTION_COUNT.try_into().map_err(|_| conv_error())?;
    if compute_buffer_header.instruction_num != expected_count {
        msg!("Incomplete compute buffer. {} of {}", compute_buffer_header.instruction_num, expected_count);
        return Err(ProgramError::InvalidArgument);
    }

    // verify that the instruction buffer is correct
    let instruction_buffer_data = instruction_buffer_info.try_borrow_data()?;
    if instruction_buffer_data[dalek::HEADER_SIZE..]
        != DSL_INSTRUCTION_BYTES
    {
        msg!("Invalid instruction buffer");
        return Err(ProgramError::InvalidArgument);
    }

    solana_program::log::sol_log_compute_units();

    /* we expect the input buffer to be laid out as the following:
     *
     * [
     *    // ..input header..
     *
     *    // equality proof statement points
     *    32 bytes:  src elgamal pubkey
     *    32 bytes:  pedersen base H compressed
     *    32 bytes:  Y_0 (b_1 * src elegamal pubkey)
     *
     *    32 bytes:  dst elgamal pubkey
     *    32 bytes:  D2_EG dst cipher text pedersen decrypt handle
     *    32 bytes:  Y_1 (b_2 * dst elegamal pubkey)
     *
     *    32 bytes:  C2_EG dst cipher text pedersen commitment
     *    32 bytes:  C1_EG src cipher text pedersen commitment
     *    32 bytes:  D1_EG src cipher text pedersen decrypt handle
     *    32 bytes:  pedersen base H compressed
     *    32 bytes:  Y_2 (b_1 * src decrypt handle - b_2 * H)
     *
     *
     *    // equality verification scalars
     *    // that s_1 is the secret key for P1_EG
     *    32 bytes:  self.sh_1
     *    32 bytes:  -c
     *    32 bytes:  -Scalar::one()
     *
     *    // that r_2 is the randomness used in D2_EG
     *    32 bytes:  self.rh_2
     *    32 bytes:  -c
     *    32 bytes:  -Scaler::one()
     *
     *    // that the messages in C1_EG and C2_EG are equal under s_1 and r_2
     *    32 bytes:  c
     *    32 bytes:  -c
     *    32 bytes:  self.sh_1
     *    32 bytes:  -self.rh_2
     *    32 bytes:  -Scaler::one()
     *
     *
     */

    let mut buffer_idx = dalek::HEADER_SIZE;
    let input_buffer_data = input_buffer_info.try_borrow_data()?;

    let equality_proof = EqualityProof::from_bytes(&transfer.proof.equality_proof.0)
        .map_err(|_| conv_error())?;

    // verify proof values are as expected
    let expected_pubkeys = [
        // statement inputs
        &transfer.transfer_public_keys.src_pubkey.0,
        &COMPRESSED_H,
        &equality_proof.Y_0.0,

        &transfer.transfer_public_keys.dst_pubkey.0,
        &transfer.dst_cipher_key_chunk_ct.0[32..],
        &equality_proof.Y_1.0,

        &transfer.dst_cipher_key_chunk_ct.0[..32],
        &transfer.src_cipher_key_chunk_ct.0[..32],
        &transfer.src_cipher_key_chunk_ct.0[32..],
        &COMPRESSED_H,
        &equality_proof.Y_2.0,
    ];
    msg!("Verifying input points");
    for i in 0..expected_pubkeys.len() {
        let found_pubkey = &input_buffer_data[buffer_idx..buffer_idx+32];
        if *found_pubkey != *expected_pubkeys[i] {
            msg!("Mismatched proof statement keys");
            return Err(StealthError::ProofVerificationError.into());
        }
        buffer_idx += 32;
    }

    solana_program::log::sol_log_compute_units();

    // same as in TransferProof::verify and EqualityProof::verify but with DSL outputs
    let mut transcript = TransferProof::transcript_new();

    TransferProof::build_transcript(
        &transfer.src_cipher_key_chunk_ct,
        &transfer.dst_cipher_key_chunk_ct,
        &transfer.transfer_public_keys,
        &mut transcript,
    ).map_err(|_| conv_error())?;

    EqualityProof::build_transcript(
        &equality_proof,
        &mut transcript,
    ).map_err(|_| conv_error())?;

    solana_program::log::sol_log_compute_units();

    msg!("Getting challenge scalars");
    let challenge_c = transcript.challenge_scalar(b"c");
    // TODO: do we need to fetch 'w'? should be deterministically after...

    solana_program::log::sol_log_compute_units();

    // verify scalars are as expected
    use curve25519_dalek::scalar::Scalar;
    let neg_challenge_c = -challenge_c;
    let neg_rh_2 = -equality_proof.rh_2;
    let neg_one = Scalar{ bytes: [
        0xEC, 0xD3, 0xF5, 0x5C, 0x1A, 0x63, 0x12, 0x58,
        0xD6, 0x9C, 0xF7, 0xA2, 0xDE, 0xF9, 0xDE, 0x14,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
    ] };
    let expected_scalars = [
         &equality_proof.sh_1,
         &neg_challenge_c,
         &neg_one,

         &equality_proof.rh_2,
         &neg_challenge_c,
         &neg_one,

         &challenge_c,
         &neg_challenge_c,
         &equality_proof.sh_1,
         &neg_rh_2,
         &neg_one,
    ];

    solana_program::log::sol_log_compute_units();

    msg!("Verifying input scalars");
    for i in 0..expected_scalars.len() {
        let mut scalar_buffer = [0; 32];
        scalar_buffer.copy_from_slice(&input_buffer_data[buffer_idx..buffer_idx+32]);
        if scalar_buffer != expected_scalars[i].bytes {
            msg!("Mismatched proof statement scalars");
            return Err(StealthError::ProofVerificationError.into());
        }
        buffer_idx += 32;
    }

    // check identity
    use curve25519_dalek_onchain::traits::Identity;
    let expected_bytes = curve25519_dalek_onchain::edwards::EdwardsPoint::identity().to_bytes();
    let found_bytes = &input_buffer_data[buffer_idx..buffer_idx+128];
    if *found_bytes != expected_bytes {
        msg!("Mismatched proof statement identity");
        return Err(ProgramError::InvalidArgument);
    }

    solana_program::log::sol_log_compute_units();

    // check that multiplication results are correct
    use curve25519_dalek::traits::IsIdentity;
    let mut buffer_idx = dalek::HEADER_SIZE;
    msg!("Verifying multiscalar mul results");
    for _i in 0..3 {
        let mul_result = curve25519_dalek::edwards::EdwardsPoint::from_bytes(
            &compute_buffer_data[buffer_idx..buffer_idx+128]
        );

        if ! curve25519_dalek::ristretto::RistrettoPoint(mul_result).is_identity() {
            msg!("Proof statement did not verify");
            return Err(StealthError::ProofVerificationError.into());
        }
        buffer_idx += 128;
    }

    transfer_buffer.updated = true.into();
    transfer_buffer.encrypted_cipher_key = transfer.dst_cipher_key_chunk_ct;


    Ok(())
}

fn process_publish_elgamal_pubkey(
    accounts: &[AccountInfo],
    elgamal_pk: &zk_token_elgamal::pod::ElGamalPubkey,
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let wallet_info = next_account_info(account_info_iter)?;
    let mint_info = next_account_info(account_info_iter)?;
    let elgamal_pubkey_info = next_account_info(account_info_iter)?;
    let system_program_info = next_account_info(account_info_iter)?;
    let rent_sysvar_info = next_account_info(account_info_iter)?;

    if !wallet_info.is_signer {
        return Err(ProgramError::InvalidArgument);
    }
    validate_account_owner(mint_info, &spl_token::ID)?;

    // check that PDA matches
    let seeds = &[
        PREFIX.as_bytes(),
        wallet_info.key.as_ref(),
        mint_info.key.as_ref(),
    ];
    let (elgamal_pubkey_key, elgamal_pubkey_bump_seed) =
        Pubkey::find_program_address(seeds, &ID);

    if elgamal_pubkey_key != *elgamal_pubkey_info.key {
        msg!("Invalid wallet elgamal PDA");
        return Err(StealthError::InvalidElgamalPubkeyPDA.into());
    }

    // create and initialize PDA
    let rent = &Rent::from_account_info(rent_sysvar_info)?;
    let space = EncryptionKeyBuffer::get_packed_len();
    invoke_signed(
        &system_instruction::create_account(
            wallet_info.key,
            elgamal_pubkey_info.key,
            rent.minimum_balance(space).max(1),
            space as u64,
            &ID,
        ),
        &[
            wallet_info.clone(),
            elgamal_pubkey_info.clone(),
            system_program_info.clone(),
        ],
        &[
            &[
                PREFIX.as_bytes(),
                wallet_info.key.as_ref(),
                mint_info.key.as_ref(),
                &[elgamal_pubkey_bump_seed],
            ],
        ],
    )?;

    let mut encryption_buffer = EncryptionKeyBuffer::from_account_info(
        &elgamal_pubkey_info, &ID, Key::Uninitialized)?.into_mut();

    encryption_buffer.key = Key::EncryptionKeyBufferV1;
    encryption_buffer.owner = *wallet_info.key;
    encryption_buffer.mint = *mint_info.key;
    encryption_buffer.elgamal_pk = *elgamal_pk;

    Ok(())
}

fn process_close_elgamal_pubkey(
    accounts: &[AccountInfo],
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let wallet_info = next_account_info(account_info_iter)?;
    let mint_info = next_account_info(account_info_iter)?;
    let elgamal_pubkey_info = next_account_info(account_info_iter)?;
    let _system_program_info = next_account_info(account_info_iter)?;

    if !wallet_info.is_signer {
        return Err(ProgramError::InvalidArgument);
    }
    validate_account_owner(mint_info, &spl_token::ID)?;
    validate_account_owner(elgamal_pubkey_info, &ID)?;

    // check that PDA matches
    let seeds = &[
        PREFIX.as_bytes(),
        wallet_info.key.as_ref(),
        mint_info.key.as_ref(),
    ];
    let (elgamal_pubkey_key, _elgamal_pubkey_bump_seed) =
        Pubkey::find_program_address(seeds, &ID);

    if elgamal_pubkey_key != *elgamal_pubkey_info.key {
        msg!("Invalid wallet elgamal PDA");
        return Err(StealthError::InvalidElgamalPubkeyPDA.into());
    }

    // close the elgamal pubkey buffer
    let starting_lamports = wallet_info.lamports();
    **wallet_info.lamports.borrow_mut() = starting_lamports
        .checked_add(elgamal_pubkey_info.lamports())
        .ok_or::<ProgramError>(StealthError::Overflow.into())?;

    **elgamal_pubkey_info.lamports.borrow_mut() = 0;

    Ok(())
}

fn validate_account_owner(account_info: &AccountInfo, owner: &Pubkey) -> ProgramResult {
    if account_info.owner == owner {
        Ok(())
    } else {
        msg!("Mismatched account owner");
        Err(ProgramError::InvalidArgument)
    }
}

fn validate_transfer_buffer(
    transfer_buffer: &CipherKeyTransferBuffer,
    stealth: &StealthAccount,
    authority: &Pubkey,
    stealth_key: &Pubkey,
) -> ProgramResult {
    if stealth.wallet_pk != *authority {
        msg!("Owner mismatch");
        return Err(ProgramError::InvalidArgument);
    }

    if transfer_buffer.stealth_key != *stealth_key {
        msg!("Stealth mismatch");
        return Err(ProgramError::InvalidArgument);
    }

    Ok(())
}

