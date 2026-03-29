import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import { TokenStaking } from "../target/types/token_staking";
import {
  Keypair,
  LAMPORTS_PER_SOL,
  PublicKey,
  SystemProgram,
  SYSVAR_RENT_PUBKEY,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  createMint,
  createAccount,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { expect } from "chai";

describe("token-staking", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace.TokenStaking as Program<TokenStaking>;

  const admin = provider.wallet as anchor.Wallet;
  const user = Keypair.generate();
  const rewardVaultKP = Keypair.generate();
  const stakeVaultKP = Keypair.generate();

  let stakeMint: PublicKey;
  let rewardMint: PublicKey;
  let adminRewardATA: PublicKey;
  let userStakeATA: PublicKey;
  let userRewardATA: PublicKey;
  let pool: PublicKey;

  const REWARD_RATE = new BN(1_000_000_000);
  const LOCK_PERIOD = new BN(2);
  const STAKE_AMOUNT = new BN(100 * 1e6);

  function userStakePDA(poolKey: PublicKey, userKey: PublicKey) {
    return PublicKey.findProgramAddressSync(
      [Buffer.from("user_stake"), poolKey.toBuffer(), userKey.toBuffer()],
      program.programId
    );
  }

  before(async () => {
    const sig = await provider.connection.requestAirdrop(user.publicKey, 10 * LAMPORTS_PER_SOL);
    await provider.connection.confirmTransaction(sig, "confirmed");

    stakeMint = await createMint(provider.connection, admin.payer, admin.publicKey, null, 6);
    rewardMint = await createMint(provider.connection, admin.payer, admin.publicKey, null, 6);

    adminRewardATA = await createAccount(provider.connection, admin.payer, rewardMint, admin.publicKey);
    userStakeATA = await createAccount(provider.connection, user, stakeMint, user.publicKey);
    userRewardATA = await createAccount(provider.connection, user, rewardMint, user.publicKey);

    await mintTo(provider.connection, admin.payer, stakeMint, userStakeATA, admin.publicKey, 1_000_000 * 1e6);
    await mintTo(provider.connection, admin.payer, rewardMint, adminRewardATA, admin.publicKey, 10_000_000 * 1e6);

    [pool] = PublicKey.findProgramAddressSync(
      [Buffer.from("pool"), stakeMint.toBuffer()],
      program.programId
    );
  });

  //  1. Initialize Pool

  it("Initializes the staking pool with correct parameters", async () => {
    await program.methods
      .initializePool(REWARD_RATE, LOCK_PERIOD)
      .accounts({
        admin: admin.publicKey,
        stakeMint,
        rewardMint,
        pool,
        rewardVault: rewardVaultKP.publicKey,
        stakeVault: stakeVaultKP.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
        rent: SYSVAR_RENT_PUBKEY,
      })
      .signers([rewardVaultKP, stakeVaultKP])
      .rpc();

    const poolData = await program.account.pool.fetch(pool);
    expect(poolData.rewardRate.toString()).to.eq(REWARD_RATE.toString());
    expect(poolData.lockPeriod.toString()).to.eq(LOCK_PERIOD.toString());
    expect(poolData.isPaused).to.be.false;
    expect(poolData.totalStaked.toString()).to.eq("0");
    expect(poolData.admin.toBase58()).to.eq(admin.publicKey.toBase58());
  });

  //  2. Fund Rewards

  it("Funds the reward vault and reflects correct balance", async () => {
    const amount = new BN(1_000_000 * 1e6);
    await program.methods
      .fundRewards(amount)
      .accounts({
        funder: admin.publicKey,
        pool,
        funderRewardAccount: adminRewardATA,
        rewardVault: rewardVaultKP.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .rpc();

    const vault = await getAccount(provider.connection, rewardVaultKP.publicKey);
    expect(vault.amount.toString()).to.eq(amount.toString());
  });

  //  3. Stake

  it("Allows a user to stake tokens and updates pool total", async () => {
    const [userStake] = userStakePDA(pool, user.publicKey);

    await program.methods
      .stake(STAKE_AMOUNT)
      .accounts({
        user: user.publicKey,
        pool,
        userStake,
        userTokenAccount: userStakeATA,
        stakeVault: stakeVaultKP.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .signers([user])
      .rpc();

    const poolData = await program.account.pool.fetch(pool);
    expect(poolData.totalStaked.toString()).to.eq(STAKE_AMOUNT.toString());

    const stakeData = await program.account.userStake.fetch(userStake);
    expect(stakeData.amount.toString()).to.eq(STAKE_AMOUNT.toString());
    expect(stakeData.owner.toBase58()).to.eq(user.publicKey.toBase58());
  });

  //  4. Claim Rewards

  it("Accrues and pays out rewards based on time staked", async () => {
    await new Promise((r) => setTimeout(r, 3000));

    const [userStake] = userStakePDA(pool, user.publicKey);
    const before = (await getAccount(provider.connection, userRewardATA)).amount;

    await program.methods
      .claimRewards()
      .accounts({
        user: user.publicKey,
        pool,
        userStake,
        rewardVault: rewardVaultKP.publicKey,
        userRewardAccount: userRewardATA,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([user])
      .rpc();

    const after = (await getAccount(provider.connection, userRewardATA)).amount;
    expect(Number(after - before)).to.be.greaterThan(0);
    console.log(`      => Rewards claimed: ${(after - before).toString()} tokens`);
  });

  //  5. Admin Pause / Unpause

  it("Admin can pause and unpause the pool", async () => {
    await program.methods.setPaused(true).accounts({ admin: admin.publicKey, pool }).rpc();
    let poolData = await program.account.pool.fetch(pool);
    expect(poolData.isPaused).to.be.true;

    await program.methods.setPaused(false).accounts({ admin: admin.publicKey, pool }).rpc();
    poolData = await program.account.pool.fetch(pool);
    expect(poolData.isPaused).to.be.false;
  });

  //  6. Reject staking when paused

  it("Rejects staking when pool is paused", async () => {
    await program.methods.setPaused(true).accounts({ admin: admin.publicKey, pool }).rpc();

    const [userStake] = userStakePDA(pool, user.publicKey);
    try {
      await program.methods
        .stake(new BN(1))
        .accounts({
          user: user.publicKey,
          pool,
          userStake,
          userTokenAccount: userStakeATA,
          stakeVault: stakeVaultKP.publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        })
        .signers([user])
        .rpc();
      expect.fail("Should have thrown PoolPaused");
    } catch (err: any) {
      expect(err.toString()).to.include("PoolPaused");
    }

    await program.methods.setPaused(false).accounts({ admin: admin.publicKey, pool }).rpc();
  });

  //  7. Reject unauthorized admin calls

  it("Rejects admin instructions from non-admin wallet", async () => {
    const attacker = Keypair.generate();
    const sig = await provider.connection.requestAirdrop(attacker.publicKey, LAMPORTS_PER_SOL);
    await provider.connection.confirmTransaction(sig, "confirmed");

    try {
      await program.methods
        .setRewardRate(new BN(9_999_999))
        .accounts({ admin: attacker.publicKey, pool })
        .signers([attacker])
        .rpc();
      expect.fail("Should have thrown Unauthorized");
    } catch (err: any) {
      expect(err.toString()).to.include("Unauthorized");
    }
  });

  // 8. Unstake after lock period

  it("Allows full unstake after lock period and zeroes pool total", async () => {
    await new Promise((r) => setTimeout(r, 3000));

    const [userStake] = userStakePDA(pool, user.publicKey);
    const stakeData = await program.account.userStake.fetch(userStake);
    const before = (await getAccount(provider.connection, userStakeATA)).amount;

    await program.methods
      .unstake(stakeData.amount)
      .accounts({
        user: user.publicKey,
        pool,
        userStake,
        userTokenAccount: userStakeATA,
        stakeVault: stakeVaultKP.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([user])
      .rpc();

    const after = (await getAccount(provider.connection, userStakeATA)).amount;
    expect(after - before).to.eq(BigInt(stakeData.amount.toString()));

    const poolData = await program.account.pool.fetch(pool);
    expect(poolData.totalStaked.toString()).to.eq("0");
  });
});