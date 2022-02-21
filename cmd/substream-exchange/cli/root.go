package cli

import (
	"context"
	"fmt"
	"io"
	"net/http"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/spf13/viper"
	"github.com/streamingfast/bstream"
	"github.com/streamingfast/bstream/firehose"
	"github.com/streamingfast/dstore"
	"github.com/streamingfast/eth-go"
	"github.com/streamingfast/eth-go/rpc"
	"github.com/streamingfast/sparkle-pancakeswap/exchange"
	"github.com/streamingfast/sparkle-pancakeswap/state"
	"github.com/streamingfast/sparkle-pancakeswap/subscription"
	"github.com/streamingfast/sparkle/indexer"
	pbcodec "github.com/streamingfast/sparkle/pb/sf/ethereum/codec/v1"
	"go.uber.org/zap"

	"github.com/spf13/cobra"
)

// rootCmd represents the base command when called without any subcommands
var rootCmd = &cobra.Command{
	Use:   "substream-pancakeswap",
	Short: "A PancakeSwap substream",
	RunE:  runRoot,
}

func runRoot(cmd *cobra.Command, args []string) error {
	ctx := cmd.Context()

	var blockCount uint64 = 1000
	if len(args) > 0 {
		val, err := strconv.ParseInt(args[0], 10, 64)
		if err != nil {
			return fmt.Errorf("invalid block count %s", args[1])
		}
		blockCount = uint64(val)
	}

	startBlockNum := viper.GetInt64("start-block")
	forceLoadState := false
	if startBlockNum > genesisBlock {
		forceLoadState = true
	}

	rpcEndpoint := os.Getenv("BSC_ENDPOINT")
	if rpcEndpoint == "" {
		rpcEndpoint = "http://localhost:8546" //  kc port-forward sub-pancake4-exchange-lucid-koschei-59686b7cc6-k49jk 8546:10.0.1.19:8546
	}

	localBlocksPath := viper.GetString("blocks-store-url")
	blocksStore, err := dstore.NewDBinStore(localBlocksPath)
	if err != nil {
		return fmt.Errorf("setting up blocks store: %w", err)
	}

	irrIndexesPath := viper.GetString("irr-indexes-url")
	irrStore, err := dstore.NewStore(irrIndexesPath, "", "", false)
	if err != nil {
		return fmt.Errorf("setting up irr blocks store: %w", err)
	}

	rpcCacheStore, err := dstore.NewStore("./rpc-cache", "", "", false)
	if err != nil {
		return fmt.Errorf("setting up store for rpc-cache: %w", err)
	}

	httpClient := &http.Client{
		Transport: &http.Transport{
			DisableKeepAlives: true, // don't reuse connections
		},
		Timeout: 3 * time.Second,
	}

	rpcClient := rpc.NewClient(rpcEndpoint, rpc.WithHttpClient(httpClient))
	rpcCache := indexer.NewCache(rpcCacheStore, rpcCacheStore, 0, 999)
	rpcCache.Load(ctx)
	intr := exchange.NewSubstreamIntrinsics(rpcClient, rpcCache, true)

	stateStorePath := viper.GetString("state-store-url")
	stateStore, err := dstore.NewStore(stateStorePath, "", "", false)
	if err != nil {
		return fmt.Errorf("setting up store for data: %w", err)
	}

	ioFactory := state.NewStoreStateIOFactory(stateStore)
	stores := map[string]*state.Builder{}
	for _, storeName := range []string{"pairs", "total_pairs", "prices", "volume24h"} {
		s := state.New(storeName, ioFactory)
		stores[storeName] = s
	}

	if forceLoadState {
		err := loadStateFromDisk(stores, uint64(startBlockNum))
		if err != nil {
			return err
		}
	}

	pipe := Pipeline{
		startBlockNum: uint64(startBlockNum),
		rpcClient:     rpcClient,
		rpcCache:      rpcCache,
		intr:          intr,
		stores:        stores,
	}

	pipe.setupSubscriptionHub()
	pipe.setupPrintPairUpdates()
	handler := pipe.handlerFactory(blockCount)

	hose := firehose.New([]dstore.Store{blocksStore}, startBlockNum, handler,
		firehose.WithForkableSteps(bstream.StepIrreversible),
		firehose.WithIrreversibleBlocksIndex(irrStore, true, []uint64{10000, 1000, 100}),
	)

	if err := hose.Run(context.Background()); err != nil {
		return fmt.Errorf("running the firehose: %w", err)
	}
	time.Sleep(5 * time.Second)

	return nil
}

type Pipeline struct {
	startBlockNum uint64

	rpcClient       *rpc.Client
	subscriptionHub *subscription.Hub
	rpcCache        *indexer.RPCCache

	intr   *exchange.SubstreamIntrinsics
	stores map[string]*state.Builder
}

func (p *Pipeline) setupSubscriptionHub() {
	// TODO: wwwooah, SubscriptionHub has a meaning in the context of bstream,
	// this would be *another* flavor SubscriptionHub? We're talking of a generic Pub/Sub here?
	//
	// Let's discuss the purpose of this thing.
	p.subscriptionHub = subscription.NewHub()

	for storeName := range p.stores {
		if err := p.subscriptionHub.RegisterTopic(storeName); err != nil {
			zlog.Fatal("pair subscriber register topic", zap.Error(err))
		}
	}

}

func (p *Pipeline) setupPrintPairUpdates() {
	pairSubscriber := subscription.NewSubscriber()
	if err := p.subscriptionHub.Subscribe(pairSubscriber, "pairs"); err != nil {
		zlog.Fatal("subscription hub subscribe", zap.Error(err))
	}

	go func() {
		for {
			delta, err := pairSubscriber.Next()
			if err != nil {
				zlog.Fatal("pair subscriber next", zap.Error(err))
			}
			if !strings.HasPrefix(delta.Key, "pair") {
				continue
			}

			p.stores["pairs"].PrintDelta(delta)

		}
	}()
	// End subscription hub

}

func (p *Pipeline) handlerFactory(blockCount uint64) bstream.Handler {
	pairExtractor := &exchange.PairExtractor{SubstreamIntrinsics: p.intr, Contract: eth.Address(exchange.FactoryAddressBytes)}
	pairsStateBuilder := &exchange.PairsStateBuilder{SubstreamIntrinsics: p.intr}
	totalPairsStateBuilder := &exchange.TotalPairsStateBuilder{SubstreamIntrinsics: p.intr}
	pricesStateBuilder := &exchange.PricesStateBuilder{SubstreamIntrinsics: p.intr}
	reservesExtractor := &exchange.ReservesExtractor{SubstreamIntrinsics: p.intr}
	swapsExtractor := &exchange.SwapsExtractor{SubstreamIntrinsics: p.intr}
	volume24hStateBuilder := &exchange.PCSVolume24hStateBuilder{SubstreamIntrinsics: p.intr}

	return bstream.HandlerFunc(func(block *bstream.Block, obj interface{}) error {

		// TODO: eventually, handle the `undo` signals.
		//  NOTE: The RUNTIME will handle the undo signals. It'll have all it needs.
		if block.Number >= p.startBlockNum+blockCount {
			//
			// FLUSH ALL THE STORES TO DISK
			// PRINT THE BLOCK NUMBER WHERE WE STOP, NEXT TIME START FROM THERE
			//
			for _, s := range p.stores {
				s.WriteState(context.Background(), block)
			}

			p.rpcCache.Save(context.Background())

			return io.EOF
		}

		blk := block.ToProtocol().(*pbcodec.Block)
		p.intr.SetCurrentBlock(blk)

		fmt.Println("-------------------------------------------------------------------")
		fmt.Printf("BLOCK +%d %d %s\n", blk.Num()-p.startBlockNum, blk.Num(), blk.ID())

		pairs, err := pairExtractor.Map(blk)
		if err != nil {
			return fmt.Errorf("extracting pairs: %w", err)
		}
		pairs.Print()

		if err := pairsStateBuilder.BuildState(pairs, p.stores["pairs"]); err != nil {
			return fmt.Errorf("processing pair cache: %w", err)
		}

		err = p.subscriptionHub.BroadcastDeltas("pairs", p.stores["pairs"].Deltas)
		if err != nil {
			return fmt.Errorf("broadcasting deltas for topic [pairs]")
		}

		p.stores["pairs"].PrintDeltas()

		reserveUpdates, err := reservesExtractor.Map(blk, p.stores["pairs"])
		if err != nil {
			return fmt.Errorf("processing reserves extractor: %w", err)
		}
		reserveUpdates.Print()

		if err := pricesStateBuilder.BuildState(reserveUpdates, p.stores["pairs"], p.stores["prices"]); err != nil {
			return fmt.Errorf("pairs price building: %w", err)
		}
		p.stores["prices"].PrintDeltas()

		swaps, err := swapsExtractor.Map(blk, p.stores["pairs"], p.stores["prices"])
		if err != nil {
			return fmt.Errorf("swaps extractor: %w", err)
		}

		swaps.Print()

		if err := totalPairsStateBuilder.BuildState(pairs, swaps, p.stores["total_pairs"]); err != nil {
			return fmt.Errorf("processing total pairs: %w", err)
		}
		p.stores["total_pairs"].PrintDeltas()

		if err := volume24hStateBuilder.BuildState(blk, swaps, p.stores["volume24h"]); err != nil {
			return fmt.Errorf("volume24 builder: %w", err)
		}

		p.stores["volume24h"].PrintDeltas()

		// Build a new "ReserveFilter{Pairs: []}"
		// followed by a AvgPriceStateBuilder
		// The idea is to replace: https://github.com/streamingfast/substream-pancakeswap/blob/master/exchange/handle_pair_sync_event.go#L249 into a stream.

		//Flush state periodically, and deltas at all blocks, on disk.
		//pairsStore.StoreBlock(context.Background(), block)
		//totalPairsStore.StoreBlock(context.Background(), block)
		//pricesStore.StoreBlock(context.Background(), block)
		//volume24hStore.StoreBlock(context.Background(), block)

		for _, s := range p.stores {
			err := s.StoreBlock(context.Background(), block)
			if err != nil {
				return err
			}
		}

		// Prep for next block, clean-up all deltas. This ought to be
		// done by the runtime, when doing clean-up between blocks.
		for _, s := range p.stores {
			s.Flush()
		}

		// MARK INDEX:
		// if len(pairs) != 0 || len(reserveUpdates) != 0 {
		// 	indexer.MarkBlock(block) // each 100 blocks y'écrit whatever
		// }

		return nil
	})
}

func loadStateFromDisk(stores map[string]*state.Builder, startBlockNum uint64) error {
	for storeName, store := range stores {
		if err := store.Init(startBlockNum); err != nil {
			return fmt.Errorf("could not load state for store %s at block num %d: %w", storeName, startBlockNum, err)
		}
	}
	return nil
}
